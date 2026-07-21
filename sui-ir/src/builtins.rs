//! L3 slice 3 — the builtins bridge: the most-used **pure** builtins
//! implemented natively on [`IrValue`], each one mirroring the tree-walker's
//! implementation (the semantic oracle) and differential-gated by
//! `tests/eval_differential.rs`.
//!
//! # Shape
//!
//! One [`IrBuiltin`] kind per builtin; partial application is uniform —
//! `IrValue::Builtin(kind, captured)` carries the arguments captured so far,
//! and [`apply_builtin`] either captures the next argument (running the same
//! per-stage validation the walker's staged closures run) or executes at
//! saturation. Display names byte-mirror the walker's `BuiltinFn::name`
//! ladder (`"map"` → `"map<partial>"`, `"foldl'"` → `"foldl'<p1>"` →
//! `"foldl'<p2>"`, `register_curried`'s `"curried<partial>"` for `split`).
//!
//! # The `builtins` attrset
//!
//! [`builtins_attrs`] mirrors the walker's registry surface: every key the
//! walker's eval-visible `builtins` set carries exists here too — the
//! implemented subset as native builtins, constants as values
//! (`storeDir` / `nixVersion` / `currentSystem` / `langVersion` /
//! `true`/`false`/`null`), the self-reference `builtins.builtins` as the
//! walker's pre-self-insert snapshot, and **every unimplemented name as a
//! pre-seeded failed thunk** carrying a typed
//! [`IrEvalError::MissingBuiltin`] — so `builtins ? x` answers like the
//! walker for the full registry, and forcing an unimplemented builtin is a
//! typed gap, never a wrong value. Key-set parity with the walker is
//! enforced by a dedicated differential test.

use std::rc::Rc;

use crate::eval_ir::{
    apply, coerce_to_string_plain, IrAttrs, IrEnv, IrEvalError, IrThunk, IrValue,
};
use crate::file_eval;

// ── the builtin kinds ─────────────────────────────────────────────────────

/// The natively-implemented pure builtin set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrBuiltin {
    // arity 1
    ToString,
    TypeOf,
    IsNull,
    IsInt,
    IsFloat,
    IsBool,
    IsString,
    IsList,
    IsAttrs,
    IsFunction,
    IsPath,
    Length,
    Head,
    Tail,
    AttrNames,
    AttrValues,
    ConcatLists,
    ListToAttrs,
    StringLength,
    Import,
    // arity 2
    Map,
    Filter,
    ElemAt,
    HasAttr,
    GetAttr,
    IntersectAttrs,
    MapAttrs,
    RemoveAttrs,
    GenList,
    Seq,
    DeepSeq,
    ConcatStringsSep,
    Split,
    // arity 3
    Foldl,
    Substring,
    ReplaceStrings,
}

impl IrBuiltin {
    /// Number of arguments before the builtin executes.
    #[must_use]
    pub fn arity(self) -> usize {
        use IrBuiltin as B;
        match self {
            B::ToString
            | B::TypeOf
            | B::IsNull
            | B::IsInt
            | B::IsFloat
            | B::IsBool
            | B::IsString
            | B::IsList
            | B::IsAttrs
            | B::IsFunction
            | B::IsPath
            | B::Length
            | B::Head
            | B::Tail
            | B::AttrNames
            | B::AttrValues
            | B::ConcatLists
            | B::ListToAttrs
            | B::StringLength
            | B::Import => 1,
            B::Map
            | B::Filter
            | B::ElemAt
            | B::HasAttr
            | B::GetAttr
            | B::IntersectAttrs
            | B::MapAttrs
            | B::RemoveAttrs
            | B::GenList
            | B::Seq
            | B::DeepSeq
            | B::ConcatStringsSep
            | B::Split => 2,
            B::Foldl | B::Substring | B::ReplaceStrings => 3,
        }
    }

    /// The builtins-attrset key this kind registers under.
    #[must_use]
    pub fn registry_name(self) -> &'static str {
        use IrBuiltin as B;
        match self {
            B::ToString => "toString",
            B::TypeOf => "typeOf",
            B::IsNull => "isNull",
            B::IsInt => "isInt",
            B::IsFloat => "isFloat",
            B::IsBool => "isBool",
            B::IsString => "isString",
            B::IsList => "isList",
            B::IsAttrs => "isAttrs",
            B::IsFunction => "isFunction",
            B::IsPath => "isPath",
            B::Length => "length",
            B::Head => "head",
            B::Tail => "tail",
            B::AttrNames => "attrNames",
            B::AttrValues => "attrValues",
            B::ConcatLists => "concatLists",
            B::ListToAttrs => "listToAttrs",
            B::StringLength => "stringLength",
            B::Import => "import",
            B::Map => "map",
            B::Filter => "filter",
            B::ElemAt => "elemAt",
            B::HasAttr => "hasAttr",
            B::GetAttr => "getAttr",
            B::IntersectAttrs => "intersectAttrs",
            B::MapAttrs => "mapAttrs",
            B::RemoveAttrs => "removeAttrs",
            B::GenList => "genList",
            B::Seq => "seq",
            B::DeepSeq => "deepSeq",
            B::ConcatStringsSep => "concatStringsSep",
            B::Split => "split",
            B::Foldl => "foldl'",
            B::Substring => "substring",
            B::ReplaceStrings => "replaceStrings",
        }
    }

    /// The walker's display name at `captured` applied arguments —
    /// byte-mirrored so both engines render partial applications
    /// identically (`<<builtin map<partial>>>`).
    #[must_use]
    pub fn display_name(self, captured: usize) -> &'static str {
        use IrBuiltin as B;
        if captured == 0 {
            return self.registry_name();
        }
        match (self, captured) {
            (B::Map, _) => "map<partial>",
            (B::Filter, _) => "filter<partial>",
            (B::ElemAt, _) => "elemAt<partial>",
            (B::HasAttr, _) => "hasAttr<partial>",
            (B::GetAttr, _) => "getAttr<partial>",
            (B::IntersectAttrs, _) => "intersectAttrs<partial>",
            (B::MapAttrs, _) => "mapAttrs<partial>",
            (B::RemoveAttrs, _) => "removeAttrs<partial>",
            (B::GenList, _) => "genList<partial>",
            (B::Seq, _) => "seq<partial>",
            (B::DeepSeq, _) => "deepSeq<partial>",
            (B::ConcatStringsSep, _) => "concatStringsSep<partial>",
            // The walker registers `split` via `register_curried`, whose
            // partial stage is anonymous.
            (B::Split, _) => "curried<partial>",
            (B::Foldl, 1) => "foldl'<p1>",
            (B::Foldl, _) => "foldl'<p2>",
            (B::Substring, 1) => "substring<p1>",
            (B::Substring, _) => "substring<p2>",
            (B::ReplaceStrings, 1) => "replaceStrings<p1>",
            (B::ReplaceStrings, _) => "replaceStrings<p2>",
            _ => self.registry_name(),
        }
    }

    /// Whether the NEXT argument must be passed **unforced** (the walker's
    /// `seq<partial>` / `deepSeq<partial>` special case in `apply_inner`).
    #[must_use]
    pub fn wants_unforced_arg(self, captured: usize) -> bool {
        matches!(self, IrBuiltin::Seq | IrBuiltin::DeepSeq) && captured == 1
    }
}

/// Every builtin kind, for registry construction.
const ALL_IMPLEMENTED: &[IrBuiltin] = &[
    IrBuiltin::ToString,
    IrBuiltin::TypeOf,
    IrBuiltin::IsNull,
    IrBuiltin::IsInt,
    IrBuiltin::IsFloat,
    IrBuiltin::IsBool,
    IrBuiltin::IsString,
    IrBuiltin::IsList,
    IrBuiltin::IsAttrs,
    IrBuiltin::IsFunction,
    IrBuiltin::IsPath,
    IrBuiltin::Length,
    IrBuiltin::Head,
    IrBuiltin::Tail,
    IrBuiltin::AttrNames,
    IrBuiltin::AttrValues,
    IrBuiltin::ConcatLists,
    IrBuiltin::ListToAttrs,
    IrBuiltin::StringLength,
    IrBuiltin::Import,
    IrBuiltin::Map,
    IrBuiltin::Filter,
    IrBuiltin::ElemAt,
    IrBuiltin::HasAttr,
    IrBuiltin::GetAttr,
    IrBuiltin::IntersectAttrs,
    IrBuiltin::MapAttrs,
    IrBuiltin::RemoveAttrs,
    IrBuiltin::GenList,
    IrBuiltin::Seq,
    IrBuiltin::DeepSeq,
    IrBuiltin::ConcatStringsSep,
    IrBuiltin::Split,
    IrBuiltin::Foldl,
    IrBuiltin::Substring,
    IrBuiltin::ReplaceStrings,
];

/// Walker builtins-set keys with NO native implementation here. Each is
/// pre-seeded as a failed thunk carrying a typed `MissingBuiltin` error so
/// `builtins ? name` matches the walker while forcing stays a typed gap.
/// (Set-parity with the walker's eval-visible registry is enforced by the
/// `builtins_registry_parity` differential test.)
const MISSING_BUILTIN_NAMES: &[&str] = &[
    "abort",
    "add",
    "addDrvOutputDependencies",
    "addErrorContext",
    "all",
    "any",
    "appendContext",
    "baseNameOf",
    "bitAnd",
    "bitOr",
    "bitXor",
    "break",
    "catAttrs",
    "ceil",
    "compareVersions",
    "concatMap",
    "concatStrings",
    "convertHash",
    "currentTime",
    "derivation",
    "derivationStrict",
    "dirOf",
    "div",
    "elem",
    "fetchGit",
    "fetchMercurial",
    "fetchTarball",
    "fetchTree",
    "fetchurl",
    "filterAttrs",
    "filterSource",
    "findFile",
    "flakeRefToString",
    "floor",
    "fromJSON",
    "fromTOML",
    "functionArgs",
    "genericClosure",
    "getContext",
    "getEnv",
    "getFlake",
    "groupBy",
    "hasContext",
    "hasPrefix",
    "hasSuffix",
    "hashFile",
    "hashString",
    "lessThan",
    "match",
    "mul",
    "nixPath",
    "parseDrvName",
    "parseFlakeRef",
    "partition",
    "path",
    "pathExists",
    "placeholder",
    "readDir",
    "readFile",
    "readFileType",
    "resolveFlakeRef",
    "scopedImport",
    "sort",
    "splitVersion",
    "storePath",
    "sub",
    "sui",
    "throw",
    "toFile",
    "toJSON",
    "toLower",
    "toPath",
    "toUpper",
    "toXML",
    "trace",
    "traceVerbose",
    "tryEval",
    "unsafeDiscardOutputDependency",
    "unsafeDiscardStringContext",
    "unsafeGetAttrPos",
    "warn",
    "zipAttrsWith",
];

/// The walker's `DEFAULT_SCOPE` — builtins CppNix exposes bare at top
/// level (mirrored from `sui-eval/src/builtins/mod.rs`).
const DEFAULT_SCOPE: &[&str] = &[
    "abort",
    "baseNameOf",
    "derivation",
    "derivationStrict",
    "dirOf",
    "false",
    "fetchGit",
    "fetchMercurial",
    "fetchTarball",
    "fetchTree",
    "fromTOML",
    "import",
    "isNull",
    "map",
    "null",
    "placeholder",
    "removeAttrs",
    "scopedImport",
    "throw",
    "toString",
    "true",
];

/// The walker's `current_system()` cfg ladder, mirrored — the "fixed
/// injected string" the differential relies on being identical on the
/// same host.
#[must_use]
pub fn current_system() -> &'static str {
    if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "aarch64-darwin"
        } else {
            "x86_64-darwin"
        }
    } else if cfg!(target_arch = "aarch64") {
        "aarch64-linux"
    } else {
        "x86_64-linux"
    }
}

/// Build the `builtins` attrset (see module docs). Mirrors the walker's
/// `builtins::register`: implemented natives + constants + missing-seeded
/// names, then the pre-self-insert snapshot as `builtins.builtins`.
#[must_use]
pub fn builtins_attrs() -> Rc<IrAttrs> {
    let mut set = IrAttrs::new();
    for kind in ALL_IMPLEMENTED {
        set.insert(
            kind.registry_name().to_string(),
            IrValue::Builtin(*kind, Rc::new(Vec::new())),
        );
    }
    // Constants (mirroring the walker's values byte-for-byte).
    set.insert("storeDir".to_string(), IrValue::string("/nix/store"));
    set.insert("nixVersion".to_string(), IrValue::string("2.34.7"));
    set.insert(
        "currentSystem".to_string(),
        IrValue::string(current_system()),
    );
    set.insert("langVersion".to_string(), IrValue::Int(6));
    set.insert("true".to_string(), IrValue::Bool(true));
    set.insert("false".to_string(), IrValue::Bool(false));
    set.insert("null".to_string(), IrValue::Null);
    for name in MISSING_BUILTIN_NAMES {
        set.insert(
            (*name).to_string(),
            IrValue::Thunk(IrThunk::failed(IrEvalError::MissingBuiltin(
                (*name).to_string(),
            ))),
        );
    }
    // `builtins.builtins` — the walker inserts a snapshot taken BEFORE the
    // self-insert, so `builtins.builtins ? builtins` is false there; mirror.
    let snapshot = IrValue::Attrs(Rc::new(set.clone()));
    set.insert("builtins".to_string(), snapshot);
    Rc::new(set)
}

/// The base environment: `builtins` + the walker's `DEFAULT_SCOPE` bare
/// bindings (implemented natives bind their native value; unimplemented
/// names bind their missing-seeded thunk).
#[must_use]
pub fn base_env() -> IrEnv {
    let attrs = builtins_attrs();
    let mut env = IrEnv::new();
    for name in DEFAULT_SCOPE {
        if let Some(v) = attrs.get(*name) {
            env.bind(name, v.clone());
        }
    }
    env.bind("builtins", IrValue::Attrs(attrs));
    env
}

// ── forced-value accessors (walker `Value::as_*` mirrors) ─────────────────

fn as_list(v: &IrValue) -> Result<&Rc<Vec<IrValue>>, IrEvalError> {
    match v {
        IrValue::List(items) => Ok(items),
        other => Err(IrEvalError::TypeMismatch {
            expected: "list",
            got: other.type_name(),
        }),
    }
}

fn as_attrs(v: &IrValue) -> Result<&Rc<IrAttrs>, IrEvalError> {
    match v {
        IrValue::Attrs(attrs) => Ok(attrs),
        other => Err(IrEvalError::TypeMismatch {
            expected: "set",
            got: other.type_name(),
        }),
    }
}

fn as_str(v: &IrValue) -> Result<&str, IrEvalError> {
    match v {
        IrValue::Str(s) => Ok(s),
        other => Err(IrEvalError::TypeMismatch {
            expected: "string",
            got: other.type_name(),
        }),
    }
}

fn as_int(v: &IrValue) -> Result<i64, IrEvalError> {
    match v {
        IrValue::Int(n) => Ok(*n),
        other => Err(IrEvalError::TypeMismatch {
            expected: "int",
            got: other.type_name(),
        }),
    }
}

/// Force a (possibly thunked) element to WHNF then require a string —
/// the walker's `Value::to_str`.
fn force_str(v: &IrValue) -> Result<String, IrEvalError> {
    let forced = v.force()?;
    as_str(&forced).map(ToOwned::to_owned)
}

/// The walker's `deep_force`: force to WHNF, recurse into attr values and
/// list elements.
fn deep_force(v: &IrValue) -> Result<(), IrEvalError> {
    let forced = v.force()?;
    match &forced {
        IrValue::Attrs(attrs) => {
            for value in attrs.values() {
                deep_force(value)?;
            }
        }
        IrValue::List(items) => {
            for item in items.iter() {
                deep_force(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

// ── application ───────────────────────────────────────────────────────────

/// Apply one argument to a builtin. `arg` arrives forced to WHNF except
/// where [`IrBuiltin::wants_unforced_arg`] said otherwise (mirroring the
/// walker's `apply_inner`). Either captures (running the walker's per-stage
/// validation) or executes at saturation.
pub fn apply_builtin(
    kind: IrBuiltin,
    captured: &[IrValue],
    arg: IrValue,
) -> Result<IrValue, IrEvalError> {
    let stage = captured.len();
    if stage + 1 < kind.arity() {
        capture_check(kind, stage, &arg)?;
        let mut caps = Vec::with_capacity(stage + 1);
        caps.extend_from_slice(captured);
        caps.push(arg);
        return Ok(IrValue::Builtin(kind, Rc::new(caps)));
    }
    run_saturated(kind, captured, arg)
}

/// Per-stage validation the walker's staged closures perform at capture
/// time (so type errors fire at the same application step on both engines).
fn capture_check(kind: IrBuiltin, stage: usize, arg: &IrValue) -> Result<(), IrEvalError> {
    use IrBuiltin as B;
    match (kind, stage) {
        // `elemAt list index`: the list is validated at stage 0.
        (B::ElemAt, 0) => as_list(arg).map(|_| ()),
        // String-first curried builtins.
        (B::HasAttr | B::GetAttr | B::ConcatStringsSep, 0) => as_str(arg).map(|_| ()),
        // Attrs-first curried builtins.
        (B::IntersectAttrs | B::RemoveAttrs, 0) => as_attrs(arg).map(|_| ()),
        (B::Substring, 0 | 1) => as_int(arg).map(|_| ()),
        // `deepSeq a b` deep-forces `a` during the FIRST application.
        (B::DeepSeq, 0) => deep_force(arg),
        // `replaceStrings from to s`: `from` elements are strict strings at
        // stage 0; `to` elements are string-coerced at stage 1.
        (B::ReplaceStrings, 0) => {
            for item in as_list(arg)?.iter() {
                force_str(item)?;
            }
            Ok(())
        }
        (B::ReplaceStrings, 1) => {
            for item in as_list(arg)?.iter() {
                coerce_to_string_plain(&item.force()?)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[allow(clippy::too_many_lines)]
fn run_saturated(
    kind: IrBuiltin,
    captured: &[IrValue],
    arg: IrValue,
) -> Result<IrValue, IrEvalError> {
    use IrBuiltin as B;
    match kind {
        B::ToString => Ok(IrValue::string(coerce_to_string_plain(&arg)?)),
        B::TypeOf => Ok(IrValue::string(arg.type_name())),
        B::IsNull => Ok(IrValue::Bool(matches!(arg, IrValue::Null))),
        B::IsInt => Ok(IrValue::Bool(matches!(arg, IrValue::Int(_)))),
        B::IsFloat => Ok(IrValue::Bool(matches!(arg, IrValue::Float(_)))),
        B::IsBool => Ok(IrValue::Bool(matches!(arg, IrValue::Bool(_)))),
        B::IsString => Ok(IrValue::Bool(matches!(arg, IrValue::Str(_)))),
        B::IsList => Ok(IrValue::Bool(matches!(arg, IrValue::List(_)))),
        B::IsAttrs => Ok(IrValue::Bool(matches!(arg, IrValue::Attrs(_)))),
        B::IsFunction => Ok(IrValue::Bool(matches!(
            arg,
            IrValue::Lambda(_) | IrValue::Builtin(..)
        ))),
        B::IsPath => Ok(IrValue::Bool(matches!(arg, IrValue::Path(_)))),
        B::Length => Ok(IrValue::Int(as_list(&arg)?.len() as i64)),
        B::Head => as_list(&arg)?
            .first()
            .cloned()
            .ok_or_else(|| IrEvalError::TypeError("head: empty list".to_string())),
        B::Tail => {
            let list = as_list(&arg)?;
            if list.is_empty() {
                return Err(IrEvalError::TypeError("tail: empty list".to_string()));
            }
            Ok(IrValue::List(Rc::new(list[1..].to_vec())))
        }
        B::AttrNames => Ok(IrValue::List(Rc::new(
            as_attrs(&arg)?
                .keys()
                .map(|k| IrValue::string(k.clone()))
                .collect(),
        ))),
        B::AttrValues => Ok(IrValue::List(Rc::new(
            as_attrs(&arg)?.values().cloned().collect(),
        ))),
        B::ConcatLists => {
            let outer = as_list(&arg)?;
            let mut result = Vec::new();
            for item in outer.iter() {
                let forced = item.force()?;
                result.extend(as_list(&forced)?.iter().cloned());
            }
            Ok(IrValue::List(Rc::new(result)))
        }
        B::ListToAttrs => {
            let list = as_list(&arg)?;
            let mut attrs = IrAttrs::new();
            for item in list.iter() {
                let forced = item.force()?;
                let item_attrs = as_attrs(&forced)?;
                let name_value = item_attrs
                    .get("name")
                    .ok_or_else(|| IrEvalError::AttrNotFound("name".to_string()))?;
                let name = force_str(name_value)?;
                let value = item_attrs
                    .get("value")
                    .ok_or_else(|| IrEvalError::AttrNotFound("value".to_string()))?
                    .clone();
                // Nix semantics: FIRST occurrence of a duplicate name wins.
                attrs.entry(name).or_insert(value);
            }
            Ok(IrValue::Attrs(Rc::new(attrs)))
        }
        B::StringLength => Ok(IrValue::Int(as_str(&arg)?.len() as i64)),
        B::Import => {
            let raw = coerce_import_path(&arg)?;
            file_eval::import(&raw)
        }
        B::Map => {
            let func = captured[0].clone();
            let list = as_list(&arg)?;
            // Lazy per-element `f elem` thunks — mirrors the walker's map.
            Ok(IrValue::List(Rc::new(
                list.iter()
                    .map(|v| {
                        IrValue::Thunk(IrThunk::native_apply(func.clone(), vec![v.clone()]))
                    })
                    .collect(),
            )))
        }
        B::Filter => {
            let pred = captured[0].clone();
            let list = as_list(&arg)?;
            let mut result = Vec::new();
            for v in list.iter() {
                if apply(pred.clone(), v.clone())?.force()?.as_bool()? {
                    result.push(v.clone());
                }
            }
            Ok(IrValue::List(Rc::new(result)))
        }
        B::ElemAt => {
            let list = as_list(&captured[0])?;
            let idx = as_int(&arg)?;
            usize::try_from(idx)
                .ok()
                .and_then(|i| list.get(i))
                .cloned()
                .ok_or_else(|| {
                    IrEvalError::TypeError(elem_at_oob(idx))
                })
        }
        B::HasAttr => {
            let name = as_str(&captured[0])?;
            Ok(IrValue::Bool(as_attrs(&arg)?.contains_key(name)))
        }
        B::GetAttr => {
            let name = as_str(&captured[0])?;
            as_attrs(&arg)?
                .get(name)
                .cloned()
                .ok_or_else(|| IrEvalError::AttrNotFound(name.to_string()))
        }
        B::IntersectAttrs => {
            let a = as_attrs(&captured[0])?;
            let b = as_attrs(&arg)?;
            let mut result = IrAttrs::new();
            for (k, v) in b.iter() {
                if a.contains_key(k) {
                    result.insert(k.clone(), v.clone());
                }
            }
            Ok(IrValue::Attrs(Rc::new(result)))
        }
        B::MapAttrs => {
            let func = captured[0].clone();
            let attrs = as_attrs(&arg)?;
            let mut result = IrAttrs::new();
            for (k, v) in attrs.iter() {
                // Lazy `f key value` thunk per entry — mirrors the walker.
                result.insert(
                    k.clone(),
                    IrValue::Thunk(IrThunk::native_apply(
                        func.clone(),
                        vec![IrValue::string(k.clone()), v.clone()],
                    )),
                );
            }
            Ok(IrValue::Attrs(Rc::new(result)))
        }
        B::RemoveAttrs => {
            let mut result = (**as_attrs(&captured[0])?).clone();
            for name in as_list(&arg)?.iter() {
                // Mirror the walker's `filter_map(to_str().ok())`: a
                // non-string (or failing) name is silently skipped.
                if let Ok(s) = force_str(name) {
                    result.remove(&s);
                }
            }
            Ok(IrValue::Attrs(Rc::new(result)))
        }
        B::GenList => {
            let func = captured[0].clone();
            let n = as_int(&arg)?;
            if n < 0 {
                return Err(IrEvalError::TypeError(genlist_negative(n)));
            }
            let mut result = Vec::with_capacity(usize::try_from(n).unwrap_or(0));
            for i in 0..n {
                result.push(apply(func.clone(), IrValue::Int(i))?);
            }
            Ok(IrValue::List(Rc::new(result)))
        }
        // `seq a b` / `deepSeq a b`: `a` was forced at capture (deepSeq
        // deeply); `b` arrives UNFORCED and is returned as-is.
        B::Seq | B::DeepSeq => Ok(arg),
        B::ConcatStringsSep => {
            let sep = as_str(&captured[0])?;
            let list = as_list(&arg)?;
            let mut result = String::new();
            for (i, v) in list.iter().enumerate() {
                if i > 0 {
                    result.push_str(sep);
                }
                result.push_str(&coerce_to_string_plain(&v.force()?)?);
            }
            Ok(IrValue::string(result))
        }
        B::Split => {
            let pattern = as_str(&captured[0])?;
            let input = as_str(&arg)?;
            split_impl(pattern, input)
        }
        B::Foldl => {
            let func = captured[0].clone();
            let mut acc = captured[1].clone();
            for v in as_list(&arg)?.iter() {
                let partial = apply(func.clone(), acc)?;
                // The walker forces the accumulator each step (CppNix's
                // forceValue(vCur)).
                acc = apply(partial, v.clone())?.force()?;
            }
            Ok(acc)
        }
        B::Substring => {
            let start_i = as_int(&captured[0])?;
            let len_i = as_int(&captured[1])?;
            let s = as_str(&arg)?;
            if start_i < 0 {
                return Err(IrEvalError::TypeError(
                    "substring: negative start position".to_string(),
                ));
            }
            let s_len = s.len();
            let start = usize::try_from(start_i).unwrap_or(usize::MAX).min(s_len);
            let end = if len_i < 0 {
                s_len
            } else {
                start
                    .saturating_add(usize::try_from(len_i).unwrap_or(usize::MAX))
                    .min(s_len)
            };
            Ok(IrValue::string(s[start..end].to_string()))
        }
        B::ReplaceStrings => {
            let from: Vec<String> = as_list(&captured[0])?
                .iter()
                .map(force_str)
                .collect::<Result<_, _>>()?;
            let to: Vec<String> = as_list(&captured[1])?
                .iter()
                .map(|v| coerce_to_string_plain(&v.force()?))
                .collect::<Result<_, _>>()?;
            let subject = coerce_to_string_plain(&arg)?;
            Ok(IrValue::string(replace_strings_impl(&from, &to, &subject)))
        }
    }
}

fn elem_at_oob(idx: i64) -> String {
    let mut s = String::from("elemAt: index ");
    s.push_str(&idx.to_string());
    s.push_str(" out of bounds");
    s
}

fn genlist_negative(n: i64) -> String {
    let mut s = String::from("genList: negative list length ");
    s.push_str(&n.to_string());
    s
}

/// The walker's `coerce_to_realized_path` restricted to the pure subset:
/// path and string values pass through; an attrset follows `outPath`
/// (derivation *realization* is unreachable here — no `derivation`).
fn coerce_import_path(v: &IrValue) -> Result<String, IrEvalError> {
    match v {
        IrValue::Path(p) => Ok((**p).clone()),
        IrValue::Str(s) => Ok((**s).clone()),
        IrValue::Attrs(attrs) => match attrs.get("outPath") {
            Some(out) => coerce_import_path(&out.force()?),
            None => Err(IrEvalError::TypeError(
                "import: expected path or string, got set without outPath".to_string(),
            )),
        },
        other => {
            let mut msg = String::from("import: expected path or string, got ");
            msg.push_str(other.type_name());
            Err(IrEvalError::TypeError(msg))
        }
    }
}

// ── split (regex, mirroring the walker byte-for-byte) ─────────────────────

fn cached_regex(pattern: &str) -> Result<regex::Regex, IrEvalError> {
    use std::cell::RefCell;
    use std::collections::HashMap;
    thread_local! {
        static REGEX_CACHE: RefCell<HashMap<String, regex::Regex>> =
            RefCell::new(HashMap::new());
    }
    REGEX_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(re) = cache.get(pattern) {
            return Ok(re.clone());
        }
        let re = regex::Regex::new(pattern).map_err(|e| {
            let mut msg = String::from("invalid regex '");
            msg.push_str(pattern);
            msg.push_str("': ");
            msg.push_str(&e.to_string());
            IrEvalError::TypeError(msg)
        })?;
        cache.insert(pattern.to_string(), re.clone());
        Ok(re)
    })
}

/// `builtins.split`, mirroring the walker's implementation exactly —
/// including its detail of re-running `captures` on the suffix at each
/// match start (byte-parity with the oracle is the gate, not CppNix).
fn split_impl(pattern: &str, input: &str) -> Result<IrValue, IrEvalError> {
    let re = cached_regex(pattern)?;
    let mut result: Vec<IrValue> = Vec::new();
    let mut last_end = 0;
    for m in re.find_iter(input) {
        result.push(IrValue::string(input[last_end..m.start()].to_string()));
        if let Some(caps) = re.captures(&input[m.start()..]) {
            let groups: Vec<IrValue> = (1..caps.len())
                .map(|i| match caps.get(i) {
                    Some(g) => IrValue::string(g.as_str().to_string()),
                    None => IrValue::Null,
                })
                .collect();
            result.push(IrValue::List(Rc::new(groups)));
        }
        last_end = m.end();
    }
    result.push(IrValue::string(input[last_end..].to_string()));
    Ok(IrValue::List(Rc::new(result)))
}

/// The walker's `replaceStrings` scan: left-to-right, first `from` match
/// wins, empty `from` entries fire at every position and at end-of-string.
fn replace_strings_impl(from: &[String], to: &[String], subject: &str) -> String {
    let bytes = subject.as_bytes();
    let mut result = String::with_capacity(subject.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let mut matched = false;
        for (idx, f) in from.iter().enumerate() {
            if !f.is_empty() && subject[i..].starts_with(f.as_str()) {
                result.push_str(&to[idx]);
                i += f.len();
                matched = true;
                break;
            }
        }
        if !matched {
            if let Some(empty_idx) = from.iter().position(String::is_empty) {
                result.push_str(&to[empty_idx]);
            }
            let ch_len = subject[i..].chars().next().map_or(1, char::len_utf8);
            result.push_str(&subject[i..i + ch_len]);
            i += ch_len;
        }
    }
    if let Some(empty_idx) = from.iter().position(String::is_empty) {
        result.push_str(&to[empty_idx]);
    }
    result
}
