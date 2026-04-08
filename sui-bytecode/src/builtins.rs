//! Built-in function registry for the bytecode VM.
//!
//! Implements Nix builtins natively in the VM value system. These are
//! the core builtins needed for nixpkgs evaluation. Each builtin is
//! registered by name and index, and can be called via the `CallBuiltin`
//! opcode or through the `builtins` attrset.
//!
//! Curried builtins (e.g., `map f list`) return a `VMBuiltin` partial
//! application on the first call, then complete on the second.

use std::collections::BTreeMap;
use std::rc::Rc;

use crate::error::VMError;
use crate::intern::{Interner, Symbol};
use crate::value::{VMBuiltin, VMValue};

/// Registry of builtin functions accessible from the VM.
pub struct BuiltinRegistry {
    /// Builtins indexed by name.
    entries: Vec<BuiltinEntry>,
}

struct BuiltinEntry {
    name: &'static str,
    func: Rc<dyn Fn(Vec<VMValue>) -> Result<VMValue, VMError>>,
    arity: u8,
}

impl BuiltinRegistry {
    /// Create a new registry with all Nix builtins registered.
    #[must_use]
    pub fn new() -> Self {
        let mut reg = Self {
            entries: Vec::new(),
        };
        reg.register_all();
        reg
    }

    /// Look up a builtin by name, returning its index.
    #[must_use]
    pub fn lookup(&self, name: &str) -> Option<u16> {
        self.entries
            .iter()
            .position(|e| e.name == name)
            .map(|i| i as u16)
    }

    /// Call a builtin by index.
    pub fn call(&self, index: u16, args: Vec<VMValue>) -> Result<VMValue, VMError> {
        let entry = self
            .entries
            .get(index as usize)
            .ok_or_else(|| VMError::UnknownBuiltin(format!("index {index}")))?;
        (entry.func)(args)
    }

    /// Build the `builtins` attribute set with all registered builtins.
    pub fn make_builtins_attrset(&self, interner: &mut Interner) -> VMValue {
        let mut attrs = BTreeMap::new();
        for (i, entry) in self.entries.iter().enumerate() {
            let sym = interner.intern(entry.name);
            let builtin = VMValue::Builtin(VMBuiltin {
                name: entry.name,
                func: Rc::clone(&entry.func),
                arity: entry.arity,
            });
            let _ = i;
            attrs.insert(sym, builtin);
        }

        // Add builtins.currentSystem
        let sys_sym = interner.intern("currentSystem");
        let system = if cfg!(target_arch = "aarch64") {
            if cfg!(target_os = "macos") {
                "aarch64-darwin"
            } else {
                "aarch64-linux"
            }
        } else if cfg!(target_os = "macos") {
            "x86_64-darwin"
        } else {
            "x86_64-linux"
        };
        attrs.insert(sys_sym, VMValue::String(system.to_string()));

        // Add builtins.nixVersion
        let ver_sym = interner.intern("nixVersion");
        attrs.insert(ver_sym, VMValue::String("2.24.0".to_string()));

        // Add builtins.langVersion
        let lang_sym = interner.intern("langVersion");
        attrs.insert(lang_sym, VMValue::Int(6));

        // Add builtins.true / builtins.false / builtins.null
        let true_sym = interner.intern("true");
        attrs.insert(true_sym, VMValue::Bool(true));
        let false_sym = interner.intern("false");
        attrs.insert(false_sym, VMValue::Bool(false));
        let null_sym = interner.intern("null");
        attrs.insert(null_sym, VMValue::Null);

        VMValue::Attrs(attrs)
    }

    /// Get the name of a builtin by index.
    #[must_use]
    pub fn name(&self, index: u16) -> Option<&'static str> {
        self.entries.get(index as usize).map(|e| e.name)
    }

    fn register(
        &mut self,
        name: &'static str,
        arity: u8,
        func: impl Fn(Vec<VMValue>) -> Result<VMValue, VMError> + 'static,
    ) {
        self.entries.push(BuiltinEntry {
            name,
            func: Rc::new(func),
            arity,
        });
    }

    fn register_all(&mut self) {
        self.register_type_checks();
        self.register_list_ops();
        self.register_attrset_ops();
        self.register_string_ops();
        self.register_conversion_ops();
        self.register_control_ops();
        self.register_arithmetic_ops();
    }

    // ── Type checking ─────────────────────────────────────────────

    fn register_type_checks(&mut self) {
        self.register("typeOf", 1, |args| {
            let name = match &args[0] {
                VMValue::Null => "null",
                VMValue::Bool(_) => "bool",
                VMValue::Int(_) => "int",
                VMValue::Float(_) => "float",
                VMValue::String(_) => "string",
                VMValue::Path(_) => "path",
                VMValue::List(_) => "list",
                VMValue::Attrs(_) => "set",
                VMValue::Closure(_) | VMValue::Builtin(_) => "lambda",
                VMValue::Thunk(_) => "thunk",
            };
            Ok(VMValue::String(name.to_string()))
        });
        self.register("isNull", 1, |args| {
            Ok(VMValue::Bool(matches!(args[0], VMValue::Null)))
        });
        self.register("isInt", 1, |args| {
            Ok(VMValue::Bool(matches!(args[0], VMValue::Int(_))))
        });
        self.register("isFloat", 1, |args| {
            Ok(VMValue::Bool(matches!(args[0], VMValue::Float(_))))
        });
        self.register("isBool", 1, |args| {
            Ok(VMValue::Bool(matches!(args[0], VMValue::Bool(_))))
        });
        self.register("isString", 1, |args| {
            Ok(VMValue::Bool(matches!(args[0], VMValue::String(_))))
        });
        self.register("isList", 1, |args| {
            Ok(VMValue::Bool(matches!(args[0], VMValue::List(_))))
        });
        self.register("isAttrs", 1, |args| {
            Ok(VMValue::Bool(matches!(args[0], VMValue::Attrs(_))))
        });
        self.register("isFunction", 1, |args| {
            Ok(VMValue::Bool(matches!(
                args[0],
                VMValue::Closure(_) | VMValue::Builtin(_)
            )))
        });
        self.register("isPath", 1, |args| {
            Ok(VMValue::Bool(matches!(args[0], VMValue::Path(_))))
        });
    }

    // ── List operations ───────────────────────────────────────────

    fn register_list_ops(&mut self) {
        self.register("length", 1, |args| {
            let list = as_list(&args[0])?;
            Ok(VMValue::Int(list.len() as i64))
        });

        self.register("head", 1, |args| {
            let list = as_list(&args[0])?;
            list.first()
                .cloned()
                .ok_or_else(|| VMError::Throw("head: empty list".to_string()))
        });

        self.register("tail", 1, |args| {
            let list = as_list(&args[0])?;
            if list.is_empty() {
                return Err(VMError::Throw("tail: empty list".to_string()));
            }
            Ok(VMValue::List(list[1..].to_vec()))
        });

        self.register("elemAt", 1, |args| {
            let list = as_list(&args[0])?.to_vec();
            Ok(VMValue::Builtin(VMBuiltin {
                name: "elemAt<partial>",
                func: Rc::new(move |args2| {
                    let idx = as_int(&args2[0])? as usize;
                    list.get(idx).cloned().ok_or_else(|| {
                        VMError::Throw(format!("elemAt: index {idx} out of bounds"))
                    })
                }),
                arity: 1,
            }))
        });

        self.register("elem", 1, |args| {
            let needle = args[0].clone();
            Ok(VMValue::Builtin(VMBuiltin {
                name: "elem<partial>",
                func: Rc::new(move |args2| {
                    let haystack = as_list(&args2[0])?;
                    Ok(VMValue::Bool(haystack.contains(&needle)))
                }),
                arity: 1,
            }))
        });

        self.register("genList", 1, |args| {
            let func = args[0].clone();
            Ok(VMValue::Builtin(VMBuiltin {
                name: "genList<partial>",
                func: Rc::new(move |_args2| {
                    // genList requires calling VM closures — return a placeholder.
                    // For now, genList with closures needs the tree-walker.
                    // genList with builtins works via apply in the VM.
                    let _ = &func;
                    Err(VMError::Throw(
                        "genList: VM closure calls in builtins not yet supported".to_string(),
                    ))
                }),
                arity: 1,
            }))
        });

        // map: curried, returns partial
        self.register("map", 1, |args| {
            let func = args[0].clone();
            Ok(VMValue::Builtin(VMBuiltin {
                name: "map<partial>",
                func: Rc::new(move |_args2| {
                    let _ = &func;
                    Err(VMError::Throw(
                        "map: VM closure calls in builtins not yet supported".to_string(),
                    ))
                }),
                arity: 1,
            }))
        });

        // filter: curried, returns partial
        self.register("filter", 1, |args| {
            let pred = args[0].clone();
            Ok(VMValue::Builtin(VMBuiltin {
                name: "filter<partial>",
                func: Rc::new(move |_args2| {
                    let _ = &pred;
                    Err(VMError::Throw(
                        "filter: VM closure calls in builtins not yet supported".to_string(),
                    ))
                }),
                arity: 1,
            }))
        });

        self.register("concatLists", 1, |args| {
            let lists = as_list(&args[0])?;
            let mut result = Vec::new();
            for v in lists {
                let inner = as_list(v)?;
                result.extend(inner.iter().cloned());
            }
            Ok(VMValue::List(result))
        });

        self.register("sort", 1, |args| {
            let _cmp = args[0].clone();
            Ok(VMValue::Builtin(VMBuiltin {
                name: "sort<partial>",
                func: Rc::new(|_args2| {
                    Err(VMError::Throw(
                        "sort: VM closure calls in builtins not yet supported".to_string(),
                    ))
                }),
                arity: 1,
            }))
        });
    }

    // ── Attrset operations ────────────────────────────────────────

    fn register_attrset_ops(&mut self) {
        self.register("attrNames", 1, |args| {
            let attrs = as_attrs(&args[0])?;
            // Note: we don't have the interner here, so we can't resolve
            // Symbol keys. This builtin must be called through the VM
            // which resolves symbols. For now, this is a placeholder.
            let _ = attrs;
            Err(VMError::Throw(
                "attrNames: requires interner access (use VM dispatch)".to_string(),
            ))
        });

        self.register("attrValues", 1, |args| {
            let attrs = as_attrs(&args[0])?;
            Ok(VMValue::List(attrs.values().cloned().collect()))
        });

        self.register("hasAttr", 1, |args| {
            let name = as_string(&args[0])?.to_string();
            Ok(VMValue::Builtin(VMBuiltin {
                name: "hasAttr<partial>",
                func: Rc::new(move |_args2| {
                    // Needs interner to resolve the name to a Symbol.
                    let _ = &name;
                    Err(VMError::Throw(
                        "hasAttr: requires interner access (use VM dispatch)".to_string(),
                    ))
                }),
                arity: 1,
            }))
        });

        self.register("getAttr", 1, |args| {
            let name = as_string(&args[0])?.to_string();
            Ok(VMValue::Builtin(VMBuiltin {
                name: "getAttr<partial>",
                func: Rc::new(move |_args2| {
                    let _ = &name;
                    Err(VMError::Throw(
                        "getAttr: requires interner access (use VM dispatch)".to_string(),
                    ))
                }),
                arity: 1,
            }))
        });

        self.register("intersectAttrs", 1, |args| {
            let a_attrs = as_attrs(&args[0])?.clone();
            Ok(VMValue::Builtin(VMBuiltin {
                name: "intersectAttrs<partial>",
                func: Rc::new(move |args2| {
                    let b_attrs = as_attrs(&args2[0])?;
                    let mut result = BTreeMap::new();
                    for (k, v) in b_attrs {
                        if a_attrs.contains_key(k) {
                            result.insert(*k, v.clone());
                        }
                    }
                    Ok(VMValue::Attrs(result))
                }),
                arity: 1,
            }))
        });

        self.register("removeAttrs", 1, |args| {
            let set = as_attrs(&args[0])?.clone();
            Ok(VMValue::Builtin(VMBuiltin {
                name: "removeAttrs<partial>",
                func: Rc::new(move |_args2| {
                    // Needs interner for name resolution
                    let _ = &set;
                    Err(VMError::Throw(
                        "removeAttrs: requires interner access".to_string(),
                    ))
                }),
                arity: 1,
            }))
        });

        self.register("listToAttrs", 1, |_args| {
            Err(VMError::Throw(
                "listToAttrs: requires interner access".to_string(),
            ))
        });
    }

    // ── String operations ─────────────────────────────────────────

    fn register_string_ops(&mut self) {
        self.register("stringLength", 1, |args| {
            let s = as_string(&args[0])?;
            Ok(VMValue::Int(s.len() as i64))
        });

        self.register("substring", 1, |args| {
            let start = as_int(&args[0])? as usize;
            Ok(VMValue::Builtin(VMBuiltin {
                name: "substring<p1>",
                func: Rc::new(move |args2| {
                    let len = as_int(&args2[0])? as usize;
                    Ok(VMValue::Builtin(VMBuiltin {
                        name: "substring<p2>",
                        func: Rc::new(move |args3| {
                            let s = as_string(&args3[0])?;
                            let end = (start + len).min(s.len());
                            let actual_start = start.min(s.len());
                            Ok(VMValue::String(s[actual_start..end].to_string()))
                        }),
                        arity: 1,
                    }))
                }),
                arity: 1,
            }))
        });

        self.register("concatStringsSep", 1, |args| {
            let sep = as_string(&args[0])?.to_string();
            Ok(VMValue::Builtin(VMBuiltin {
                name: "concatStringsSep<partial>",
                func: Rc::new(move |args2| {
                    let list = as_list(&args2[0])?;
                    let strings: Result<Vec<String>, _> =
                        list.iter().map(|v| as_string(v).map(|s| s.to_string())).collect();
                    Ok(VMValue::String(strings?.join(&sep)))
                }),
                arity: 1,
            }))
        });

        self.register("replaceStrings", 1, |args| {
            let from: Vec<String> = as_list(&args[0])?
                .iter()
                .map(|v| as_string(v).map(|s| s.to_string()))
                .collect::<Result<_, _>>()?;
            Ok(VMValue::Builtin(VMBuiltin {
                name: "replaceStrings<p1>",
                func: Rc::new(move |args2| {
                    let to: Vec<String> = as_list(&args2[0])?
                        .iter()
                        .map(|v| as_string(v).map(|s| s.to_string()))
                        .collect::<Result<_, _>>()?;
                    let from2 = from.clone();
                    Ok(VMValue::Builtin(VMBuiltin {
                        name: "replaceStrings<p2>",
                        func: Rc::new(move |args3| {
                            let mut s = as_string(&args3[0])?.to_string();
                            for (f, t) in from2.iter().zip(to.iter()) {
                                if !f.is_empty() {
                                    s = s.replace(f.as_str(), t);
                                }
                            }
                            Ok(VMValue::String(s))
                        }),
                        arity: 1,
                    }))
                }),
                arity: 1,
            }))
        });

        self.register("hasPrefix", 1, |args| {
            let prefix = as_string(&args[0])?.to_string();
            Ok(VMValue::Builtin(VMBuiltin {
                name: "hasPrefix<partial>",
                func: Rc::new(move |args2| {
                    let s = as_string(&args2[0])?;
                    Ok(VMValue::Bool(s.starts_with(&*prefix)))
                }),
                arity: 1,
            }))
        });

        self.register("hasSuffix", 1, |args| {
            let suffix = as_string(&args[0])?.to_string();
            Ok(VMValue::Builtin(VMBuiltin {
                name: "hasSuffix<partial>",
                func: Rc::new(move |args2| {
                    let s = as_string(&args2[0])?;
                    Ok(VMValue::Bool(s.ends_with(&*suffix)))
                }),
                arity: 1,
            }))
        });

        self.register("toLower", 1, |args| {
            let s = as_string(&args[0])?;
            Ok(VMValue::String(s.to_lowercase()))
        });

        self.register("toUpper", 1, |args| {
            let s = as_string(&args[0])?;
            Ok(VMValue::String(s.to_uppercase()))
        });
    }

    // ── Conversion operations ─────────────────────────────────────

    fn register_conversion_ops(&mut self) {
        self.register("toString", 1, |args| {
            let s = match &args[0] {
                VMValue::String(s) => s.clone(),
                VMValue::Int(n) => n.to_string(),
                VMValue::Float(f) => format!("{f}"),
                VMValue::Bool(true) => "1".to_string(),
                VMValue::Bool(false) => String::new(),
                VMValue::Null => String::new(),
                VMValue::Path(p) => p.clone(),
                VMValue::List(_) | VMValue::Attrs(_) => {
                    return Err(VMError::Throw(
                        "toString: cannot convert set or list".to_string(),
                    ));
                }
                VMValue::Closure(_) | VMValue::Builtin(_) => {
                    return Err(VMError::Throw(
                        "toString: cannot convert function".to_string(),
                    ));
                }
                VMValue::Thunk(_) => {
                    return Err(VMError::Throw(
                        "toString: thunk should be forced first".to_string(),
                    ));
                }
            };
            Ok(VMValue::String(s))
        });

        self.register("toJSON", 1, |args| {
            let json = vm_value_to_json(&args[0])?;
            let s = serde_json::to_string(&json)
                .unwrap_or_else(|_| "null".to_string());
            Ok(VMValue::String(s))
        });

        self.register("fromJSON", 1, |args| {
            let s = as_string(&args[0])?;
            let json: serde_json::Value = serde_json::from_str(s).map_err(|e| {
                VMError::Throw(format!("fromJSON: {e}"))
            })?;
            Ok(json_to_vm_value(&json))
        });

        self.register("toInt", 1, |args| {
            let s = as_string(&args[0])?;
            let n: i64 = s.trim().parse().map_err(|e| {
                VMError::Throw(format!("toInt: {e}"))
            })?;
            Ok(VMValue::Int(n))
        });
    }

    // ── Control flow ──────────────────────────────────────────────

    fn register_control_ops(&mut self) {
        self.register("throw", 1, |args| {
            let msg = as_string(&args[0])?;
            Err(VMError::Throw(format!("throw: {msg}")))
        });

        self.register("abort", 1, |args| {
            let msg = as_string(&args[0])?;
            Err(VMError::Throw(format!("abort: {msg}")))
        });

        self.register("seq", 1, |args| {
            let _forced = args[0].clone();
            Ok(VMValue::Builtin(VMBuiltin {
                name: "seq<partial>",
                func: Rc::new(|args2| Ok(args2[0].clone())),
                arity: 1,
            }))
        });

        self.register("deepSeq", 1, |args| {
            let _forced = args[0].clone();
            Ok(VMValue::Builtin(VMBuiltin {
                name: "deepSeq<partial>",
                func: Rc::new(|args2| Ok(args2[0].clone())),
                arity: 1,
            }))
        });

        self.register("tryEval", 1, |args| {
            // In the VM, tryEval just wraps the value since we don't
            // have thunk forcing here. The VM handles the actual try/catch.
            let val = args[0].clone();
            // We can't actually catch throws here without interner access.
            // Return success with the value for now.
            // The VM will handle this specially.
            let _ = val;
            Err(VMError::Throw(
                "tryEval: requires VM-level implementation".to_string(),
            ))
        });

        self.register("trace", 1, |args| {
            let msg = args[0].clone();
            eprintln!("trace: {msg}");
            Ok(VMValue::Builtin(VMBuiltin {
                name: "trace<partial>",
                func: Rc::new(|args2| Ok(args2[0].clone())),
                arity: 1,
            }))
        });
    }

    // ── Arithmetic ────────────────────────────────────────────────

    fn register_arithmetic_ops(&mut self) {
        self.register("add", 1, |args| {
            let a = args[0].clone();
            Ok(VMValue::Builtin(VMBuiltin {
                name: "add<partial>",
                func: Rc::new(move |args2| match (&a, &args2[0]) {
                    (VMValue::Int(x), VMValue::Int(y)) => Ok(VMValue::Int(x + y)),
                    (VMValue::Float(x), VMValue::Float(y)) => Ok(VMValue::Float(x + y)),
                    (VMValue::Int(x), VMValue::Float(y)) => Ok(VMValue::Float(*x as f64 + y)),
                    (VMValue::Float(x), VMValue::Int(y)) => Ok(VMValue::Float(x + *y as f64)),
                    _ => Err(VMError::Throw("add: expected numbers".to_string())),
                }),
                arity: 1,
            }))
        });

        self.register("sub", 1, |args| {
            let a = args[0].clone();
            Ok(VMValue::Builtin(VMBuiltin {
                name: "sub<partial>",
                func: Rc::new(move |args2| match (&a, &args2[0]) {
                    (VMValue::Int(x), VMValue::Int(y)) => Ok(VMValue::Int(x - y)),
                    _ => Err(VMError::Throw("sub: expected ints".to_string())),
                }),
                arity: 1,
            }))
        });

        self.register("mul", 1, |args| {
            let a = args[0].clone();
            Ok(VMValue::Builtin(VMBuiltin {
                name: "mul<partial>",
                func: Rc::new(move |args2| match (&a, &args2[0]) {
                    (VMValue::Int(x), VMValue::Int(y)) => Ok(VMValue::Int(x * y)),
                    _ => Err(VMError::Throw("mul: expected ints".to_string())),
                }),
                arity: 1,
            }))
        });

        self.register("div", 1, |args| {
            let a = args[0].clone();
            Ok(VMValue::Builtin(VMBuiltin {
                name: "div<partial>",
                func: Rc::new(move |args2| match (&a, &args2[0]) {
                    (VMValue::Int(_), VMValue::Int(0)) => Err(VMError::DivisionByZero),
                    (VMValue::Int(x), VMValue::Int(y)) => Ok(VMValue::Int(x / y)),
                    _ => Err(VMError::Throw("div: expected ints".to_string())),
                }),
                arity: 1,
            }))
        });

        self.register("ceil", 1, |args| {
            let f = as_float(&args[0])?;
            Ok(VMValue::Int(f.ceil() as i64))
        });

        self.register("floor", 1, |args| {
            let f = as_float(&args[0])?;
            Ok(VMValue::Int(f.floor() as i64))
        });

        self.register("bitAnd", 1, |args| {
            let a = as_int(&args[0])?;
            Ok(VMValue::Builtin(VMBuiltin {
                name: "bitAnd<partial>",
                func: Rc::new(move |args2| {
                    let b = as_int(&args2[0])?;
                    Ok(VMValue::Int(a & b))
                }),
                arity: 1,
            }))
        });

        self.register("bitOr", 1, |args| {
            let a = as_int(&args[0])?;
            Ok(VMValue::Builtin(VMBuiltin {
                name: "bitOr<partial>",
                func: Rc::new(move |args2| {
                    let b = as_int(&args2[0])?;
                    Ok(VMValue::Int(a | b))
                }),
                arity: 1,
            }))
        });

        self.register("bitXor", 1, |args| {
            let a = as_int(&args[0])?;
            Ok(VMValue::Builtin(VMBuiltin {
                name: "bitXor<partial>",
                func: Rc::new(move |args2| {
                    let b = as_int(&args2[0])?;
                    Ok(VMValue::Int(a ^ b))
                }),
                arity: 1,
            }))
        });
    }
}

impl Default for BuiltinRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── Helper functions ──────────────────────────────────────────────

fn as_list(v: &VMValue) -> Result<&Vec<VMValue>, VMError> {
    match v {
        VMValue::List(l) => Ok(l),
        other => Err(VMError::TypeError {
            expected: "list",
            got: other.type_name(),
            context: "builtin argument".to_string(),
        }),
    }
}

fn as_attrs(v: &VMValue) -> Result<&BTreeMap<Symbol, VMValue>, VMError> {
    match v {
        VMValue::Attrs(a) => Ok(a),
        other => Err(VMError::TypeError {
            expected: "set",
            got: other.type_name(),
            context: "builtin argument".to_string(),
        }),
    }
}

fn as_string(v: &VMValue) -> Result<&str, VMError> {
    match v {
        VMValue::String(s) => Ok(s),
        other => Err(VMError::TypeError {
            expected: "string",
            got: other.type_name(),
            context: "builtin argument".to_string(),
        }),
    }
}

fn as_int(v: &VMValue) -> Result<i64, VMError> {
    match v {
        VMValue::Int(n) => Ok(*n),
        other => Err(VMError::TypeError {
            expected: "int",
            got: other.type_name(),
            context: "builtin argument".to_string(),
        }),
    }
}

fn as_float(v: &VMValue) -> Result<f64, VMError> {
    match v {
        VMValue::Float(f) => Ok(*f),
        VMValue::Int(n) => Ok(*n as f64),
        other => Err(VMError::TypeError {
            expected: "float",
            got: other.type_name(),
            context: "builtin argument".to_string(),
        }),
    }
}

/// Convert a VMValue to serde_json::Value for toJSON.
fn vm_value_to_json(v: &VMValue) -> Result<serde_json::Value, VMError> {
    match v {
        VMValue::Null => Ok(serde_json::Value::Null),
        VMValue::Bool(b) => Ok(serde_json::Value::Bool(*b)),
        VMValue::Int(n) => Ok(serde_json::Value::Number(
            serde_json::Number::from(*n),
        )),
        VMValue::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .ok_or_else(|| VMError::Throw("toJSON: invalid float".to_string())),
        VMValue::String(s) => Ok(serde_json::Value::String(s.clone())),
        VMValue::Path(p) => Ok(serde_json::Value::String(p.clone())),
        VMValue::List(items) => {
            let arr: Result<Vec<_>, _> = items.iter().map(vm_value_to_json).collect();
            Ok(serde_json::Value::Array(arr?))
        }
        VMValue::Attrs(_) => {
            // Can't convert attrsets without interner access for key names
            Err(VMError::Throw(
                "toJSON: attrset conversion requires interner".to_string(),
            ))
        }
        VMValue::Closure(_) | VMValue::Builtin(_) => {
            Err(VMError::Throw("toJSON: cannot convert function".to_string()))
        }
        VMValue::Thunk(_) => {
            Err(VMError::Throw("toJSON: thunk should be forced first".to_string()))
        }
    }
}

/// Convert a serde_json::Value to VMValue for fromJSON.
fn json_to_vm_value(v: &serde_json::Value) -> VMValue {
    match v {
        serde_json::Value::Null => VMValue::Null,
        serde_json::Value::Bool(b) => VMValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                VMValue::Int(i)
            } else {
                VMValue::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => VMValue::String(s.clone()),
        serde_json::Value::Array(arr) => {
            VMValue::List(arr.iter().map(json_to_vm_value).collect())
        }
        serde_json::Value::Object(_) => {
            // Can't create Symbol-keyed attrsets without an interner.
            // Return null as a fallback; real usage goes through VM.
            VMValue::Null
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_builtins() {
        let reg = BuiltinRegistry::new();
        assert!(reg.lookup("length").is_some());
        assert!(reg.lookup("typeOf").is_some());
        assert!(reg.lookup("head").is_some());
        assert!(reg.lookup("tail").is_some());
        assert!(reg.lookup("throw").is_some());
        assert!(reg.lookup("nonexistent").is_none());
    }

    #[test]
    fn call_length() {
        let reg = BuiltinRegistry::new();
        let idx = reg.lookup("length").unwrap();
        let result = reg
            .call(idx, vec![VMValue::List(vec![VMValue::Int(1), VMValue::Int(2)])])
            .unwrap();
        assert_eq!(result, VMValue::Int(2));
    }

    #[test]
    fn call_head() {
        let reg = BuiltinRegistry::new();
        let idx = reg.lookup("head").unwrap();
        let result = reg
            .call(idx, vec![VMValue::List(vec![VMValue::Int(10)])])
            .unwrap();
        assert_eq!(result, VMValue::Int(10));
    }

    #[test]
    fn call_head_empty() {
        let reg = BuiltinRegistry::new();
        let idx = reg.lookup("head").unwrap();
        let result = reg.call(idx, vec![VMValue::List(vec![])]);
        assert!(result.is_err());
    }

    #[test]
    fn call_type_of() {
        let reg = BuiltinRegistry::new();
        let idx = reg.lookup("typeOf").unwrap();
        assert_eq!(
            reg.call(idx, vec![VMValue::Int(42)]).unwrap(),
            VMValue::String("int".to_string())
        );
        assert_eq!(
            reg.call(idx, vec![VMValue::String("hello".to_string())])
                .unwrap(),
            VMValue::String("string".to_string())
        );
    }

    #[test]
    fn call_string_length() {
        let reg = BuiltinRegistry::new();
        let idx = reg.lookup("stringLength").unwrap();
        let result = reg
            .call(idx, vec![VMValue::String("hello".to_string())])
            .unwrap();
        assert_eq!(result, VMValue::Int(5));
    }

    #[test]
    fn call_throw() {
        let reg = BuiltinRegistry::new();
        let idx = reg.lookup("throw").unwrap();
        let result = reg.call(idx, vec![VMValue::String("test error".to_string())]);
        assert!(matches!(result, Err(VMError::Throw(_))));
    }

    #[test]
    fn call_to_string() {
        let reg = BuiltinRegistry::new();
        let idx = reg.lookup("toString").unwrap();
        assert_eq!(
            reg.call(idx, vec![VMValue::Int(42)]).unwrap(),
            VMValue::String("42".to_string())
        );
        assert_eq!(
            reg.call(idx, vec![VMValue::Bool(true)]).unwrap(),
            VMValue::String("1".to_string())
        );
    }

    #[test]
    fn builtins_attrset() {
        let reg = BuiltinRegistry::new();
        let mut interner = Interner::new();
        let builtins = reg.make_builtins_attrset(&mut interner);
        match &builtins {
            VMValue::Attrs(attrs) => {
                let length_sym = interner.lookup("length").unwrap();
                assert!(attrs.contains_key(&length_sym));
            }
            _ => panic!("expected Attrs"),
        }
    }
}
