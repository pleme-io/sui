//! Built-in function registry for the bytecode VM.
//!
//! Implements Nix builtins natively in the VM value system. These are
//! the core builtins needed for nixpkgs evaluation. Each builtin is
//! registered by name and index, and can be called via the `CallBuiltin`
//! opcode or through the `builtins` attrset.
//!
//! Curried builtins (e.g., `map f list`) return a `VMBuiltin` partial
//! application on the first call, then complete on the second.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;

use crate::error::VMError;
use crate::intern::{Interner, Symbol};
use crate::value::{HigherOrderBuiltin, HigherOrderOp, ThunkState, VMBuiltin, VMValue};

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

        // Add builtins.storeDir
        let store_sym = interner.intern("storeDir");
        attrs.insert(store_sym, VMValue::String("/nix/store".to_string()));

        // Add builtins.nixPath from NIX_PATH environment variable
        let nixpath_sym = interner.intern("nixPath");
        let nix_path_list = {
            let nix_path = std::env::var("NIX_PATH").unwrap_or_default();
            let entries: Vec<VMValue> = nix_path
                .split(':')
                .filter(|s| !s.is_empty())
                .map(|entry| {
                    let (prefix, path) = if let Some(idx) = entry.find('=') {
                        (entry[..idx].to_string(), entry[idx + 1..].to_string())
                    } else {
                        (String::new(), entry.to_string())
                    };
                    let prefix_sym = interner.intern("prefix");
                    let path_sym = interner.intern("path");
                    let mut entry_attrs = BTreeMap::new();
                    entry_attrs.insert(prefix_sym, VMValue::String(prefix));
                    entry_attrs.insert(path_sym, VMValue::String(path));
                    VMValue::Attrs(entry_attrs)
                })
                .collect();
            VMValue::List(entries)
        };
        attrs.insert(nixpath_sym, nix_path_list);

        // Add builtins.currentTime (0 in pure eval mode)
        let time_sym = interner.intern("currentTime");
        attrs.insert(time_sym, VMValue::Int(0));

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
        self.register_higher_order_ops();
        self.register_attrset_ops();
        self.register_string_ops();
        self.register_conversion_ops();
        self.register_control_ops();
        self.register_arithmetic_ops();
        self.register_derivation_ops();
        self.register_missing_builtins();
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
                VMValue::Closure(_) | VMValue::Builtin(_) | VMValue::HigherOrderBuiltin(_) => "lambda",
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
                VMValue::Closure(_) | VMValue::Builtin(_) | VMValue::HigherOrderBuiltin(_)
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
            Ok(VMValue::HigherOrderBuiltin(HigherOrderBuiltin {
                op: HigherOrderOp::Elem,
                func: Box::new(needle),
                extra_args: Vec::new(),
            }))
        });

        self.register("genList", 1, |args| {
            Ok(VMValue::HigherOrderBuiltin(HigherOrderBuiltin {
                op: HigherOrderOp::GenList,
                func: Box::new(args[0].clone()),
                extra_args: Vec::new(),
            }))
        });

        // map: curried, returns partial (VM handles closure calling)
        self.register("map", 1, |args| {
            Ok(VMValue::HigherOrderBuiltin(HigherOrderBuiltin {
                op: HigherOrderOp::Map,
                func: Box::new(args[0].clone()),
                extra_args: Vec::new(),
            }))
        });

        // filter: curried, returns partial (VM handles closure calling)
        self.register("filter", 1, |args| {
            Ok(VMValue::HigherOrderBuiltin(HigherOrderBuiltin {
                op: HigherOrderOp::Filter,
                func: Box::new(args[0].clone()),
                extra_args: Vec::new(),
            }))
        });

        self.register("concatLists", 1, |args| {
            let lists = as_list(&args[0])?;
            let mut result = Vec::new();
            for v in &lists {
                let inner = as_list(v)?;
                result.extend(inner);
            }
            Ok(VMValue::List(result))
        });

        self.register("sort", 1, |args| {
            Ok(VMValue::HigherOrderBuiltin(HigherOrderBuiltin {
                op: HigherOrderOp::Sort,
                func: Box::new(args[0].clone()),
                extra_args: Vec::new(),
            }))
        });
    }


    // ── Higher-order operations (need VM access) ─────────────────

    fn register_higher_order_ops(&mut self) {
        self.register("foldl'", 1, |args| {
            Ok(VMValue::HigherOrderBuiltin(HigherOrderBuiltin {
                op: HigherOrderOp::FoldlP1,
                func: Box::new(args[0].clone()),
                extra_args: Vec::new(),
            }))
        });
        self.register("concatMap", 1, |args| {
            Ok(VMValue::HigherOrderBuiltin(HigherOrderBuiltin {
                op: HigherOrderOp::ConcatMap,
                func: Box::new(args[0].clone()),
                extra_args: Vec::new(),
            }))
        });
        self.register("any", 1, |args| {
            Ok(VMValue::HigherOrderBuiltin(HigherOrderBuiltin {
                op: HigherOrderOp::Any,
                func: Box::new(args[0].clone()),
                extra_args: Vec::new(),
            }))
        });
        self.register("all", 1, |args| {
            Ok(VMValue::HigherOrderBuiltin(HigherOrderBuiltin {
                op: HigherOrderOp::All,
                func: Box::new(args[0].clone()),
                extra_args: Vec::new(),
            }))
        });
        self.register("partition", 1, |args| {
            Ok(VMValue::HigherOrderBuiltin(HigherOrderBuiltin {
                op: HigherOrderOp::Partition,
                func: Box::new(args[0].clone()),
                extra_args: Vec::new(),
            }))
        });
        self.register("groupBy", 1, |args| {
            Ok(VMValue::HigherOrderBuiltin(HigherOrderBuiltin {
                op: HigherOrderOp::GroupBy,
                func: Box::new(args[0].clone()),
                extra_args: Vec::new(),
            }))
        });
        self.register("mapAttrs", 1, |args| {
            Ok(VMValue::HigherOrderBuiltin(HigherOrderBuiltin {
                op: HigherOrderOp::MapAttrs,
                func: Box::new(args[0].clone()),
                extra_args: Vec::new(),
            }))
        });
        self.register("filterAttrs", 1, |args| {
            Ok(VMValue::HigherOrderBuiltin(HigherOrderBuiltin {
                op: HigherOrderOp::FilterAttrs,
                func: Box::new(args[0].clone()),
                extra_args: Vec::new(),
            }))
        });
        self.register("functionArgs", 1, |args| {
            match &args[0] {
                VMValue::Builtin(_) | VMValue::HigherOrderBuiltin(_) => {
                    Ok(VMValue::Attrs(BTreeMap::new()))
                }
                VMValue::Closure(closure) => {
                    let mut result = BTreeMap::new();
                    let mut interner = crate::intern::Interner::new();
                    for (name, has_default) in &closure.formals {
                        let sym = interner.intern(name);
                        result.insert(sym, VMValue::Bool(*has_default));
                    }
                    Ok(VMValue::Attrs(result))
                }
                other => Err(VMError::TypeError {
                    expected: "lambda",
                    got: other.type_name(),
                    context: "functionArgs".to_string(),
                }),
            }
        });
        self.register("catAttrs", 1, |args| {
            let name = as_string(&args[0])?.to_string();
            Ok(VMValue::Builtin(VMBuiltin {
                name: "catAttrs<partial>",
                func: Rc::new(move |args2| {
                    let list = force_as_list(&args2[0])?;
                    let mut result: Vec<VMValue> = Vec::new();
                    for item in &list {
                        if let VMValue::Attrs(_) = item {
                            let _ = &name;
                        }
                    }
                    Err(VMError::Throw(
                        "catAttrs: requires interner access (use VM dispatch)".to_string(),
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

        // attrValues needs the VM's interner to resolve Symbol keys to
        // their string names for lex-sorting — which is what CppNix
        // semantics require. Symbol itself is an intern-order u32,
        // NOT a lex-sorted key, so BTreeMap's native iteration is
        // intern-order and wrong whenever any transitive eval
        // (e.g. nixpkgs/lib) has interned a "later" key before an
        // "earlier" one. Route through VM dispatch the same way
        // attrNames does. Placeholder error that the VM recognizes.
        //
        // Discovered while probing sui against real nixpkgs:
        // `(import <nixpkgs>/lib).attrsets.mapAttrsToList
        //    (n: v: "${n}=${toString v}") { a = 1; b = 2; }`
        // returned `[ "b=2" "a=1" ]` instead of `[ "a=1" "b=2" ]`.
        self.register("attrValues", 1, |_args| {
            Err(VMError::Throw(
                "attrValues: requires interner access (use VM dispatch)".to_string(),
            ))
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
            // CppNix semantics (verified against 2.33):
            //   - negative `len` means "to end of string"
            //   - negative `start` yields empty string
            //   - out-of-range start clamps; out-of-range end clamps
            //
            // sui's VM was previously casting `i64 as usize` immediately,
            // which turned `-1` (a common CppNix convention for "rest of
            // string", used by lib.strings.removePrefix) into usize::MAX
            // and panicked with "begin <= end" on the arithmetic overflow.
            // Discovered while probing `(import <nixpkgs>/lib).strings
            //   .removePrefix "foo-" "foo-bar"` — fifth silent/loud bug
            // of the session.
            let start_i = as_int(&args[0])?;
            Ok(VMValue::Builtin(VMBuiltin {
                name: "substring<p1>",
                func: Rc::new(move |args2| {
                    let len_i = as_int(&args2[0])?;
                    Ok(VMValue::Builtin(VMBuiltin {
                        name: "substring<p2>",
                        func: Rc::new(move |args3| {
                            let s = as_string(&args3[0])?;
                            if start_i < 0 {
                                return Err(VMError::Throw(
                                    "substring: negative start position".to_string(),
                                ));
                            }
                            let s_len = s.len();
                            let start = (start_i as usize).min(s_len);
                            let end = if len_i < 0 {
                                s_len
                            } else {
                                start.saturating_add(len_i as usize).min(s_len)
                            };
                            Ok(VMValue::String(s[start..end].to_string()))
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
                    let list = force_as_list(&args2[0])?;
                    let strings: Result<Vec<String>, _> =
                        list.iter().map(|v| force_as_string(v)).collect();
                    Ok(VMValue::String(strings?.join(&sep)))
                }),
                arity: 1,
            }))
        });

        self.register("replaceStrings", 1, |args| {
            let from: Vec<String> = force_as_list(&args[0])?
                .iter()
                .map(|v| force_as_string(v))
                .collect::<Result<_, _>>()?;
            Ok(VMValue::Builtin(VMBuiltin {
                name: "replaceStrings<p1>",
                func: Rc::new(move |args2| {
                    let to: Vec<String> = force_as_list(&args2[0])?
                        .iter()
                        .map(|v| force_as_string(v))
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
            vm_coerce_to_string(&args[0])
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

    // ── Derivation ────────────────────────────────────────────────

    fn register_derivation_ops(&mut self) {
        // Both `derivation` and `derivationStrict` delegate to the same impl.
        // The actual implementation is at the VM level (vm_build_derivation)
        // because it needs interner access. These stubs are intercepted by
        // try_vm_builtin before they execute.
        self.register("derivation", 1, |_args| {
            Err(VMError::Throw(
                "derivation: requires VM-level dispatch".to_string(),
            ))
        });
        self.register("derivationStrict", 1, |_args| {
            Err(VMError::Throw(
                "derivationStrict: requires VM-level dispatch".to_string(),
            ))
        });
        // getFlake: VM-level dispatch (needs import mechanism).
        self.register("getFlake", 1, |_args| {
            Err(VMError::Throw(
                "getFlake: requires VM-level dispatch".to_string(),
            ))
        });
        // scopedImport: VM-level dispatch (needs import + interner).
        self.register("scopedImport", 1, |_args| {
            Err(VMError::Throw(
                "scopedImport: requires VM-level dispatch".to_string(),
            ))
        });

        // ── Missing builtins needed for nixpkgs lib ─────────────────

        // addErrorContext: in eval mode just returns the value (no-op wrapper)
        self.register("addErrorContext", 1, |args| {
            // Curried: addErrorContext context value → value
            Ok(VMValue::Builtin(VMBuiltin {
                name: "addErrorContext<partial>",
                func: Rc::new(move |inner_args: Vec<VMValue>| Ok(inner_args[0].clone())),
                arity: 1,
            }))
        });

        // unsafeGetAttrPos: returns null (position info not tracked in VM)
        self.register("unsafeGetAttrPos", 1, |_args| {
            Ok(VMValue::Builtin(VMBuiltin {
                name: "unsafeGetAttrPos<partial>",
                func: Rc::new(|_args: Vec<VMValue>| Ok(VMValue::Null)),
                arity: 1,
            }))
        });

        // pathExists: check if a path exists on the filesystem
        self.register("pathExists", 1, |args| {
            let path = match &args[0] {
                VMValue::Path(p) => p.clone(),
                VMValue::String(s) => s.clone(),
                other => {
                    return Err(VMError::TypeError {
                        expected: "path or string",
                        got: other.type_name(),
                        context: "pathExists".to_string(),
                    })
                }
            };
            Ok(VMValue::Bool(std::path::Path::new(&path).exists()))
        });

        // readFile: read contents of a file
        self.register("readFile", 1, |args| {
            let path = match &args[0] {
                VMValue::Path(p) => p.clone(),
                VMValue::String(s) => s.clone(),
                other => {
                    return Err(VMError::TypeError {
                        expected: "path or string",
                        got: other.type_name(),
                        context: "readFile".to_string(),
                    })
                }
            };
            let content = std::fs::read_to_string(&path)
                .map_err(|e| VMError::Throw(format!("readFile {path}: {e}")))?;
            Ok(VMValue::String(content))
        });

        // readDir: list directory entries
        self.register("readDir", 1, |args| {
            let path = match &args[0] {
                VMValue::Path(p) => p.clone(),
                VMValue::String(s) => s.clone(),
                other => {
                    return Err(VMError::TypeError {
                        expected: "path or string",
                        got: other.type_name(),
                        context: "readDir".to_string(),
                    })
                }
            };
            // Return empty attrs for now (VM doesn't have interner access here)
            Err(VMError::Throw(
                "readDir: requires VM-level dispatch for interner access".to_string(),
            ))
        });

        // baseNameOf: extract filename from a path
        self.register("baseNameOf", 1, |args| {
            let path = match &args[0] {
                VMValue::Path(p) => p.clone(),
                VMValue::String(s) => s.clone(),
                other => {
                    return Err(VMError::TypeError {
                        expected: "path or string",
                        got: other.type_name(),
                        context: "baseNameOf".to_string(),
                    })
                }
            };
            let base = std::path::Path::new(&path)
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_default();
            Ok(VMValue::String(base))
        });

        // dirOf: extract directory from a path
        self.register("dirOf", 1, |args| {
            let path = match &args[0] {
                VMValue::Path(p) => p.clone(),
                VMValue::String(s) => s.clone(),
                other => {
                    return Err(VMError::TypeError {
                        expected: "path or string",
                        got: other.type_name(),
                        context: "dirOf".to_string(),
                    })
                }
            };
            let dir = std::path::Path::new(&path)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| ".".to_string());
            Ok(VMValue::String(dir))
        });

        // genericClosure: transitive closure computation
        self.register("genericClosure", 1, |_args| {
            Err(VMError::Throw(
                "genericClosure: requires VM-level dispatch".to_string(),
            ))
        });

        // placeholder: returns placeholder string for derivation outputs
        self.register("placeholder", 1, |args| {
            let output = match &args[0] {
                VMValue::String(s) => s.clone(),
                _ => "out".to_string(),
            };
            Ok(VMValue::String(format!("/1rz4g4znpzjwh1xymhjpm42vipw92pr73vdgl6xs1hycac8kf2n9/{output}")))
        });

        // split: regex split (requires VM-level dispatch for interner)
        self.register("split", 1, |args| {
            let _pattern = as_string(&args[0])?;
            Ok(VMValue::Builtin(VMBuiltin {
                name: "split<partial>",
                func: Rc::new(|_inner_args: Vec<VMValue>| {
                    Err(VMError::Throw("split: requires VM-level dispatch".to_string()))
                }),
                arity: 1,
            }))
        });

        // match: regex match (requires VM-level dispatch for interner)
        self.register("match", 1, |args| {
            let _pattern = as_string(&args[0])?;
            Ok(VMValue::Builtin(VMBuiltin {
                name: "match<partial>",
                func: Rc::new(|_inner_args: Vec<VMValue>| {
                    Err(VMError::Throw("match: requires VM-level dispatch".to_string()))
                }),
                arity: 1,
            }))
        });

        // fromTOML: parse a TOML string
        self.register("fromTOML", 1, |args| {
            let s = as_string(&args[0])?;
            // Simple stub - would need full TOML parser
            Err(VMError::Throw(format!("fromTOML: not yet implemented")))
        });

        // concatStrings: concatenate a list of strings (used by nixpkgs lib)
        // Note: This isn't strictly a Nix builtin but is sometimes needed
        // In Nix it's actually builtins.concatStringsSep "" (already registered)

        // storeDir: the Nix store directory
        // This is a constant, added in make_builtins_attrset

        // fetchurl, fetchTarball, fetchGit, fetchTree stubs
        self.register("fetchurl", 1, |_args| {
            Err(VMError::Throw("fetchurl: not supported in eval mode".to_string()))
        });
        self.register("fetchTarball", 1, |_args| {
            Err(VMError::Throw("fetchTarball: not supported in eval mode".to_string()))
        });
        self.register("fetchGit", 1, |_args| {
            Err(VMError::Throw("fetchGit: not supported in eval mode".to_string()))
        });
        self.register("fetchTree", 1, |_args| {
            Err(VMError::Throw("fetchTree: not supported in eval mode".to_string()))
        });
        self.register("fetchMercurial", 1, |_args| {
            Err(VMError::Throw("fetchMercurial: not supported in eval mode".to_string()))
        });

        // toFile: write a file to the Nix store (stub)
        self.register("toFile", 1, |_args| {
            Err(VMError::Throw("toFile: not supported in eval mode".to_string()))
        });

        // toPath: convert string to path (deprecated in Nix, but used)
        self.register("toPath", 1, |args| {
            let s = as_string(&args[0])?;
            Ok(VMValue::Path(s.to_string()))
        });

        // import: as a builtin value (not a special form)
        // Already handled at the compiler level via OpCode::Import

        // parseDrvName: parse a derivation name-version string
        self.register("parseDrvName", 1, |args| {
            let name = as_string(&args[0])?;
            // Split at last hyphen followed by a digit
            let mut split_pos = None;
            let bytes = name.as_bytes();
            for i in (0..bytes.len()).rev() {
                if bytes[i] == b'-' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
                    split_pos = Some(i);
                    break;
                }
            }
            match split_pos {
                Some(pos) => {
                    Err(VMError::Throw(
                        "parseDrvName: requires VM-level dispatch for interner access".to_string(),
                    ))
                }
                None => {
                    Err(VMError::Throw(
                        "parseDrvName: requires VM-level dispatch for interner access".to_string(),
                    ))
                }
            }
        });

        // compareVersions — delegates to sui_compat::versions so the
        // tree-walker and VM stay in lock-step.  The previous naive
        // local implementation (split on `.` only, no `pre` handling)
        // diverged from cppnix on every nixpkgs version probe.
        self.register("compareVersions", 1, |args| {
            let a = as_string(&args[0])?.to_string();
            Ok(VMValue::Builtin(VMBuiltin {
                name: "compareVersions<partial>",
                func: Rc::new(move |inner_args: Vec<VMValue>| {
                    let b = as_string(&inner_args[0])?;
                    Ok(VMValue::Int(
                        sui_compat::versions::compare_versions(&a, b),
                    ))
                }),
                arity: 1,
            }))
        });

        // splitVersion — delegates to sui_compat::versions for the
        // same reason compareVersions does.
        self.register("splitVersion", 1, |args| {
            let version = as_string(&args[0])?;
            let parts: Vec<VMValue> = sui_compat::versions::split_version(version)
                .into_iter()
                .map(VMValue::String)
                .collect();
            Ok(VMValue::List(parts))
        });

        // concatStrings is used internally
        self.register("concatStrings", 1, |args| {
            let list = as_list(&args[0])?;
            let mut result = String::new();
            for item in &list {
                let item = force_vmvalue(item.clone()).unwrap_or_else(|_| item.clone());
                match &item {
                    VMValue::String(s) => result.push_str(s),
                    _ => {
                        return Err(VMError::TypeError {
                            expected: "string",
                            got: item.type_name(),
                            context: "concatStrings element".to_string(),
                        })
                    }
                }
            }
            Ok(VMValue::String(result))
        });

        // ── String context builtins (no-ops in eval mode) ────────────
        // Nix string contexts track derivation dependencies. In eval-only
        // mode, strings have no context, so these are identity/no-ops.
        self.register("unsafeDiscardStringContext", 1, |args| {
            // Just return the string as-is (no context to discard).
            Ok(args[0].clone())
        });
        self.register("getContext", 1, |_args| {
            // No context in eval mode — return empty attrset.
            // Need VM dispatch for interner.
            Err(VMError::Throw(
                "getContext: requires VM-level dispatch for interner access".to_string(),
            ))
        });
        self.register("appendContext", 1, |args| {
            // No context to append — return string as-is.
            Ok(VMValue::Builtin(VMBuiltin {
                name: "appendContext<partial>",
                func: Rc::new(move |inner_args: Vec<VMValue>| Ok(inner_args[0].clone())),
                arity: 1,
            }))
        });
        self.register("hasContext", 1, |_args| {
            Ok(VMValue::Bool(false))
        });
        self.register("unsafeDiscardOutputDependency", 1, |args| {
            Ok(args[0].clone())
        });
        self.register("addDrvOutputDependencies", 1, |args| {
            Ok(args[0].clone())
        });

        // ── Path/string conversion builtins ──────────────────────────
        self.register("storePath", 1, |args| {
            Ok(args[0].clone())
        });
        self.register("isStorePath", 1, |args| {
            let s = match &args[0] {
                VMValue::String(s) => s.as_str(),
                VMValue::Path(p) => p.as_str(),
                _ => return Ok(VMValue::Bool(false)),
            };
            Ok(VMValue::Bool(s.starts_with("/nix/store/")))
        });
        self.register("hashString", 1, |args| {
            let algo = as_string(&args[0])?.to_string();
            Ok(VMValue::Builtin(VMBuiltin {
                name: "hashString<partial>",
                func: Rc::new(move |inner_args: Vec<VMValue>| {
                    let s = as_string(&inner_args[0])?;
                    match algo.as_str() {
                        "sha256" => {
                            use sha2::{Sha256, Digest};
                            let mut hasher = Sha256::new();
                            hasher.update(s.as_bytes());
                            let result = hasher.finalize();
                            let hex: String = result
                                .iter()
                                .map(|b| format!("{b:02x}"))
                                .collect();
                            Ok(VMValue::String(hex))
                        }
                        _ => Err(VMError::Throw(format!("hashString: unsupported algorithm: {algo}")))
                    }
                }),
                arity: 1,
            }))
        });
        self.register("hashFile", 1, |_args| {
            Err(VMError::Throw("hashFile: not supported in eval mode".to_string()))
        });

        // import as a value (not the special form in Apply).
        // When used as `import path`, the compiler handles it via OpCode::Import.
        // But when `import` is passed as a function value (e.g., `map import paths`),
        // it needs to be callable. The VM dispatches this specially.
        self.register("import", 1, |_args| {
            Err(VMError::Throw(
                "import: requires VM-level dispatch".to_string(),
            ))
        });

        // ── Misc builtins needed by nixpkgs lib ─────────────────────
        self.register("zipAttrsWith", 1, |_args| {
            Err(VMError::Throw(
                "zipAttrsWith: requires VM-level dispatch".to_string(),
            ))
        });
    }

    // ── Missing builtins: direct implementations + bridge stubs ────
    //
    // These are builtins that the tree-walker has but the VM was missing.
    // Simple ones are implemented directly; complex ones delegate to the
    // builtin bridge (which calls back into the tree-walker).

    fn register_missing_builtins(&mut self) {
        // ── Direct implementations (simple, no tree-walker state) ────

        // getEnv: look up environment variable (returns "" if unset)
        self.register("getEnv", 1, |args| {
            let name = as_string(&args[0])?;
            let val = std::env::var(name).unwrap_or_default();
            Ok(VMValue::String(val))
        });

        // readFileType: return file type as string
        self.register("readFileType", 1, |args| {
            let path = match &args[0] {
                VMValue::Path(p) => p.clone(),
                VMValue::String(s) => s.clone(),
                other => {
                    return Err(VMError::TypeError {
                        expected: "path or string",
                        got: other.type_name(),
                        context: "readFileType".to_string(),
                    });
                }
            };
            match std::fs::symlink_metadata(&path) {
                Ok(meta) => {
                    let kind = if meta.is_symlink() {
                        "symlink"
                    } else if meta.is_dir() {
                        "directory"
                    } else if meta.is_file() {
                        "regular"
                    } else {
                        "unknown"
                    };
                    Ok(VMValue::String(kind.to_string()))
                }
                Err(e) => Err(VMError::Throw(format!("readFileType {path}: {e}"))),
            }
        });

        // findFile: curried, search NIX_PATH entries for a file
        self.register("findFile", 1, |args| {
            let search_path = as_list(&args[0])?.clone();
            Ok(VMValue::Builtin(VMBuiltin {
                name: "findFile<partial>",
                func: Rc::new(move |args2| {
                    let name = as_string(&args2[0])?;
                    for entry in &search_path {
                        if let VMValue::Attrs(a) = entry {
                            // We need the interner to look up "prefix" and "path" keys.
                            // Since this is a bridge builtin, delegate to the bridge.
                            // But first try a string-key lookup on a best-effort basis.
                            // The bridge will handle the real implementation.
                            let _ = a;
                        }
                    }
                    // Delegate to bridge for proper implementation
                    bridge_call("findFile", vec![
                        VMValue::List(search_path.clone()),
                        VMValue::String(name.to_string()),
                    ])
                }),
                arity: 1,
            }))
        });

        // lessThan: curried comparison (missing from VM arithmetic ops)
        self.register("lessThan", 1, |args| {
            let a = args[0].clone();
            Ok(VMValue::Builtin(VMBuiltin {
                name: "lessThan<partial>",
                func: Rc::new(move |args2| match (&a, &args2[0]) {
                    (VMValue::Int(x), VMValue::Int(y)) => Ok(VMValue::Bool(*x < *y)),
                    (VMValue::Float(x), VMValue::Float(y)) => Ok(VMValue::Bool(*x < *y)),
                    (VMValue::Int(x), VMValue::Float(y)) => {
                        Ok(VMValue::Bool((*x as f64) < *y))
                    }
                    (VMValue::Float(x), VMValue::Int(y)) => {
                        Ok(VMValue::Bool(*x < (*y as f64)))
                    }
                    (VMValue::String(x), VMValue::String(y)) => Ok(VMValue::Bool(*x < *y)),
                    _ => Err(VMError::Throw(
                        "lessThan: expected comparable types".to_string(),
                    )),
                }),
                arity: 1,
            }))
        });

        // warn: like trace, prints warning and returns identity
        self.register("warn", 1, |args| {
            if let Ok(msg) = as_string(&args[0]) {
                eprintln!("evaluation warning: {msg}");
            }
            Ok(VMValue::Builtin(VMBuiltin {
                name: "warn<partial>",
                func: Rc::new(|args2| Ok(args2[0].clone())),
                arity: 1,
            }))
        });

        // traceVerbose: like trace but only when SUI_TRACE_VERBOSE=1
        self.register("traceVerbose", 1, |args| {
            if std::env::var("SUI_TRACE_VERBOSE").ok().as_deref() == Some("1") {
                eprintln!("trace: {}", args[0]);
            }
            Ok(VMValue::Builtin(VMBuiltin {
                name: "traceVerbose<partial>",
                func: Rc::new(|args2| Ok(args2[0].clone())),
                arity: 1,
            }))
        });

        // break: debug breakpoint, just returns its argument
        self.register("break", 1, |args| Ok(args[0].clone()));

        // ── Bridge-delegating stubs ─────────────────────────────────
        //
        // These builtins are complex (need tree-walker state, regex cache,
        // TOML parser, hash algorithms, etc.) and are delegated to the
        // builtin bridge which calls back into the tree-walker.

        // Names of builtins that should be bridged and their arities.
        // When called, they convert args to StringKeyedValue, call the
        // bridge, and convert back.
        //
        // Note: Some of these are already registered above as stubs that
        // throw "requires VM-level dispatch". The bridge versions below
        // replace the error with actual functionality when a bridge is set.
        // We register them with unique names to avoid conflicts, and the
        // VM's try_vm_builtin handles dispatch.

        // Bridge complex builtins to tree-walker.
        // These need tree-walker state, complex algorithms, or I/O.
        for name in &["convertHash", "toXML", "toFile", "filterSource",
                      "fetchClosure", "outputOf", "hashFile", "hashString"]
        {
            let n = (*name).to_string();
            self.register(name, 1, move |args| {
                bridge_call(&n, args.to_vec())
            });
        }
    }
}

/// Helper: delegate a builtin call to the tree-walker bridge.
///
/// Converts `VMValue` args to `StringKeyedValue`, calls the bridge,
/// and converts the result back. Returns an error if no bridge is set.
fn bridge_call(name: &str, args: Vec<VMValue>) -> Result<VMValue, VMError> {
    use crate::intern::Interner;
    // Convert VMValue args to StringKeyedValue (interner-free).
    // For this we need a temporary interner to resolve any Symbol keys.
    let tmp_interner = Interner::new();
    let sk_args: Vec<crate::value::StringKeyedValue> = args
        .iter()
        .map(|a| a.to_string_keyed(&tmp_interner))
        .collect();

    match crate::bridge::call_builtin_bridge(name, sk_args) {
        Ok(Some(result)) => Ok(string_keyed_to_vmvalue(&result, &mut Interner::new())),
        Ok(None) => Err(VMError::Throw(format!(
            "builtin '{name}' requires bridge but no bridge is set"
        ))),
        Err(e) => Err(VMError::Throw(e)),
    }
}

/// Convert a `StringKeyedValue` back to a `VMValue`.
///
/// Requires an interner to create Symbol keys for attrsets.
/// Public so the VM's `try_vm_builtin` can use it for bridge dispatch.
pub fn string_keyed_to_vmvalue(
    sk: &crate::value::StringKeyedValue,
    interner: &mut crate::intern::Interner,
) -> VMValue {
    use crate::value::StringKeyedValue;
    match sk {
        StringKeyedValue::Null => VMValue::Null,
        StringKeyedValue::Bool(b) => VMValue::Bool(*b),
        StringKeyedValue::Int(n) => VMValue::Int(*n),
        StringKeyedValue::Float(f) => VMValue::Float(*f),
        StringKeyedValue::String(s) => VMValue::String(s.clone()),
        StringKeyedValue::Path(p) => VMValue::Path(p.clone()),
        StringKeyedValue::List(items) => VMValue::List(
            items
                .iter()
                .map(|v| string_keyed_to_vmvalue(v, interner))
                .collect(),
        ),
        StringKeyedValue::Attrs(map) => {
            let mut attrs = BTreeMap::new();
            for (k, v) in map {
                let sym = interner.intern(k);
                attrs.insert(sym, string_keyed_to_vmvalue(v, interner));
            }
            VMValue::Attrs(attrs)
        }
        StringKeyedValue::Lambda => VMValue::Null,
        StringKeyedValue::Callable(cb) => {
            let cb_clone = Rc::clone(cb);
            VMValue::Builtin(crate::value::VMBuiltin {
                name: "<bridge-fn>",
                arity: 1,
                func: Rc::new(move |args: Vec<VMValue>| {
                    let interner = crate::intern::Interner::new();
                    let sk_arg = args.into_iter().next()
                        .unwrap_or(VMValue::Null)
                        .to_string_keyed(&interner);
                    let sk_result = cb_clone(sk_arg)
                        .map_err(|e| VMError::Throw(e))?;
                    let mut tmp_interner = crate::intern::Interner::new();
                    Ok(string_keyed_to_vmvalue(&sk_result, &mut tmp_interner))
                }),
            })
        }
        StringKeyedValue::Thunk(cb) => {
            // Wrap the StringKeyedValue thunk as a VMThunk.
            let cb_clone = Rc::clone(cb);
            VMValue::Thunk(crate::value::VMThunk::new_native(move || {
                let sk_val = cb_clone().map_err(|e| VMError::Throw(e))?;
                // Use a fresh interner for the result conversion.
                let mut tmp = crate::intern::Interner::new();
                Ok(string_keyed_to_vmvalue(&sk_val, &mut tmp))
            }))
        }
    }
}

impl Default for BuiltinRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── Helper functions ──────────────────────────────────────────────

/// Try to extract a concrete value from a `Done` thunk without VM access.
/// Returns the inner value for already-evaluated thunks. For non-thunks,
/// returns `None` (use the value directly). For pending thunks, returns
/// an error that will cause the VM to fall back to the tree-walker.
fn try_unwrap_done_thunk(v: &VMValue) -> Option<Result<VMValue, VMError>> {
    match v {
        VMValue::Thunk(thunk) => {
            let state = thunk.state.take();
            match state {
                Some(ThunkState::Done(boxed)) => {
                    let inner = *boxed.clone();
                    thunk.state.set(Some(ThunkState::Done(boxed)));
                    // Recursively unwrap in case the result is itself a Done thunk.
                    match &inner {
                        VMValue::Thunk(_) => Some(try_unwrap_done_thunk(&inner)
                            .unwrap_or(Ok(inner))),
                        _ => Some(Ok(inner)),
                    }
                }
                other => {
                    thunk.state.set(other);
                    Some(Err(VMError::TypeError {
                        expected: "concrete value",
                        got: "thunk (pending)",
                        context: "builtin argument (thunk needs VM to force)".to_string(),
                    }))
                }
            }
        }
        _ => None, // Not a thunk — caller uses value directly
    }
}

/// Extract a list, forcing thunks if needed. Returns an owned Vec
/// because thunk forcing may produce a value we can't borrow.
fn as_list(v: &VMValue) -> Result<Vec<VMValue>, VMError> {
    match v {
        VMValue::List(l) => Ok(l.clone()),
        VMValue::Thunk(_) => {
            let forced = force_vmvalue(v.clone())?;
            match forced {
                VMValue::List(l) => Ok(l),
                other => Err(VMError::TypeError {
                    expected: "list",
                    got: other.type_name(),
                    context: "builtin argument".to_string(),
                }),
            }
        }
        other => Err(VMError::TypeError {
            expected: "list",
            got: other.type_name(),
            context: "builtin argument".to_string(),
        }),
    }
}

/// Force a VMValue if it's a thunk, returning the resolved value.
/// Handles Done thunks directly, NativeCallback via bridge, and
/// Pending thunks cause a fallback error.
fn force_vmvalue(v: VMValue) -> Result<VMValue, VMError> {
    match v {
        VMValue::Thunk(ref thunk) => {
            let state = thunk.state.take();
            match state {
                Some(ThunkState::Done(boxed)) => {
                    let inner = *boxed.clone();
                    thunk.state.set(Some(ThunkState::Done(boxed)));
                    force_vmvalue(inner) // Recursively unwrap
                }
                Some(ThunkState::NativeCallback(cb)) => {
                    thunk.state.set(Some(ThunkState::Evaluating));
                    match cb() {
                        Ok(sk_val) => {
                            // Convert StringKeyedValue back to VMValue
                            let result = sk_to_vmvalue(&sk_val);
                            thunk.state.set(Some(ThunkState::Done(Box::new(result.clone()))));
                            force_vmvalue(result)
                        }
                        Err(e) => {
                            thunk.state.set(Some(ThunkState::NativeCallback(cb)));
                            Err(VMError::Throw(e))
                        }
                    }
                }
                other => {
                    thunk.state.set(other);
                    // Pending/LazySource/Evaluating — needs VM to force.
                    Err(VMError::TypeError {
                        expected: "concrete value",
                        got: "thunk (pending)",
                        context: "builtin argument (thunk needs VM to force)".to_string(),
                    })
                }
            }
        }
        other => Ok(other),
    }
}

/// Convert StringKeyedValue → VMValue (inverse of to_string_keyed).
fn sk_to_vmvalue(sk: &crate::value::StringKeyedValue) -> VMValue {
    use crate::value::StringKeyedValue;
    match sk {
        StringKeyedValue::Null => VMValue::Null,
        StringKeyedValue::Bool(b) => VMValue::Bool(*b),
        StringKeyedValue::Int(n) => VMValue::Int(*n),
        StringKeyedValue::Float(f) => VMValue::Float(*f),
        StringKeyedValue::String(s) => VMValue::String(s.clone()),
        StringKeyedValue::Path(p) => VMValue::Path(p.clone()),
        StringKeyedValue::List(items) => {
            VMValue::List(items.iter().map(|i| sk_to_vmvalue(i)).collect())
        }
        StringKeyedValue::Attrs(map) => {
            // Use the global interner for symbol resolution
            let mut interner = crate::intern::Interner::new();
            VMValue::Attrs(map.iter().map(|(k, v)| {
                (interner.intern(k), sk_to_vmvalue(v))
            }).collect())
        }
        StringKeyedValue::Lambda => VMValue::Null, // Can't reconstruct closures
        StringKeyedValue::Thunk(cb) => {
            // Wrap as a NativeCallback VMThunk for lazy evaluation
            let cb = cb.clone();
            VMValue::Thunk(crate::value::VMThunk {
                state: Rc::new(Cell::new(Some(ThunkState::NativeCallback(cb)))),
            })
        }
        StringKeyedValue::Callable(_) => VMValue::Null, // Can't reconstruct
    }
}

fn as_attrs(v: &VMValue) -> Result<&BTreeMap<Symbol, VMValue>, VMError> {
    match v {
        VMValue::Attrs(a) => Ok(a),
        VMValue::Thunk(_) => Err(VMError::TypeError {
            expected: "set",
            got: "thunk",
            context: "builtin argument".to_string(),
        }),
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

/// Force-aware string extraction: forces thunks before extracting.
/// Use this when iterating over list elements that may be thunks.
fn force_as_string(v: &VMValue) -> Result<String, VMError> {
    match v {
        VMValue::String(s) => Ok(s.clone()),
        VMValue::Thunk(_) => {
            let forced = force_vmvalue(v.clone())?;
            match forced {
                VMValue::String(s) => Ok(s),
                other => Err(VMError::TypeError {
                    expected: "string",
                    got: other.type_name(),
                    context: "builtin argument (after forcing thunk)".to_string(),
                }),
            }
        }
        other => Err(VMError::TypeError {
            expected: "string",
            got: other.type_name(),
            context: "builtin argument".to_string(),
        }),
    }
}

/// Force-aware list extraction: forces thunks before extracting.
fn force_as_list(v: &VMValue) -> Result<Vec<VMValue>, VMError> {
    match v {
        VMValue::List(l) => Ok(l.clone()),
        VMValue::Thunk(_) => {
            let forced = force_vmvalue(v.clone())?;
            match forced {
                VMValue::List(l) => Ok(l),
                other => Err(VMError::TypeError {
                    expected: "list",
                    got: other.type_name(),
                    context: "builtin argument (after forcing thunk)".to_string(),
                }),
            }
        }
        other => Err(VMError::TypeError {
            expected: "list",
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

/// Coerce a VMValue to string, matching CppNix's `builtins.toString` semantics:
/// - Strings, ints, floats, bools, null, paths: straightforward conversion
/// - Attrsets with `__toString`: call the function with the attrset as argument
///   (handled by VM fallback — here we just check `outPath`)
/// - Attrsets with `outPath`: coerce the outPath value
/// - Lists: space-join coerced elements
fn vm_coerce_to_string(v: &VMValue) -> Result<VMValue, VMError> {
    match v {
        VMValue::String(s) => Ok(VMValue::String(s.clone())),
        VMValue::Int(n) => Ok(VMValue::String(n.to_string())),
        // 6-decimal fixed-point to match CppNix's `%f` float coercion.
        VMValue::Float(f) => Ok(VMValue::String(format!("{f:.6}"))),
        VMValue::Bool(true) => Ok(VMValue::String("1".to_string())),
        VMValue::Bool(false) => Ok(VMValue::String(String::new())),
        VMValue::Null => Ok(VMValue::String(String::new())),
        VMValue::Path(p) => Ok(VMValue::String(p.clone())),
        VMValue::Attrs(attrs) => {
            // Check __toString first (requires calling a function — if present,
            // we fall back to the VM bridge for now)
            let to_str_sym = crate::intern::intern("__toString");
            if attrs.contains_key(&to_str_sym) {
                // __toString requires calling a closure with the attrset.
                // This can't be done from a pure builtin — the VM will handle
                // this via the bridge fallback.
                return Err(VMError::Throw(
                    "toString: __toString requires VM bridge".to_string(),
                ));
            }
            let out_path_sym = crate::intern::intern("outPath");
            if let Some(out_path) = attrs.get(&out_path_sym) {
                vm_coerce_to_string(out_path)
            } else {
                Err(VMError::Throw(
                    "cannot coerce a set to a string, but it has no __toString or outPath".to_string(),
                ))
            }
        }
        VMValue::List(items) => {
            let mut parts = Vec::with_capacity(items.len());
            for item in items {
                match vm_coerce_to_string(item)? {
                    VMValue::String(s) => parts.push(s),
                    _ => unreachable!("vm_coerce_to_string always returns String"),
                }
            }
            Ok(VMValue::String(parts.join(" ")))
        }
        VMValue::Closure(_) | VMValue::Builtin(_) | VMValue::HigherOrderBuiltin(_) => {
            Err(VMError::Throw(
                "cannot coerce a function to a string".to_string(),
            ))
        }
        VMValue::Thunk(_) => {
            Err(VMError::Throw(
                "toString: thunk should be forced first".to_string(),
            ))
        }
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
        VMValue::Closure(_) | VMValue::Builtin(_) | VMValue::HigherOrderBuiltin(_) => {
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
