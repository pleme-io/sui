//! L3 slice 4 — the builtins bridge: the **pure** builtin surface
//! implemented natively on [`IrValue`], each one mirroring the tree-walker's
//! implementation (the semantic oracle) and differential-gated by
//! `tests/eval_differential.rs`.
//!
//! Slice 4 completed the pure surface (throw/abort/tryEval, sort/genericClosure/
//! functionArgs, all/any/partition/groupBy/concatMap/elem/catAttrs/zipAttrsWith/
//! filterAttrs, match/compareVersions/splitVersion/parseDrvName, toJSON/fromJSON/
//! toXML, add/sub/mul/div/lessThan/bitAnd/bitOr/bitXor/ceil/floor, concatStrings/
//! toLower/toUpper/hasPrefix/hasSuffix/hasContext/getContext/unsafeDiscard-
//! StringContext, baseNameOf/dirOf, trace/traceVerbose, findFile, the `nixPath`
//! constant). What stays a typed [`IrEvalError::MissingBuiltin`] gap is the
//! store-/IO-/derivation-/flake-/crypto-bound set (impure by nature) plus a few
//! deferred context/passthrough helpers — see [`MISSING_BUILTIN_NAMES`].
//!
//! The version algorithms and the CppNix float format come from the SAME typed
//! `sui_compat::versions` the walker uses, and toJSON/fromJSON build/parse the
//! SAME `serde_json::Value`, so those surfaces cannot drift from the oracle.
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

// The version algorithms + CppNix float format are the SAME typed
// implementations the tree-walker uses (`sui_compat::versions`), so
// `compareVersions` / `parseDrvName` / `splitVersion` cannot drift from the
// oracle by construction. `sui-compat` is a regular dependency (it has no
// pleme-io deps, so no cycle).
use sui_compat::versions::{
    compare_versions, cppnix_format_float, parse_drv_name, split_version,
};

use crate::eval_ir::{
    apply, coerce_to_string_plain, ir_eq, IrAttrs, IrContextElem, IrEnv, IrEvalError, IrThunk,
    IrValue,
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
    // arity 1 — slice 4
    Throw,
    Abort,
    TryEval,
    Ceil,
    Floor,
    ToJson,
    FromJson,
    ToXml,
    FunctionArgs,
    GenericClosure,
    SplitVersion,
    ParseDrvName,
    HasContext,
    GetContext,
    UnsafeDiscardStringContext,
    BaseNameOf,
    DirOf,
    // arity 1 — slice 6 (derivation + crypto)
    Derivation,
    DerivationStrict,
    ConvertHash,
    // probe: pure-fs readers (mirror the walker's paths.rs)
    PathExists,
    ReadFile,
    ReadDir,
    ReadFileType,
    GetEnv,
    Placeholder,
    AddErrorContext,
    // PROBE STUB — returns Null, NOT the walker's real {file,line,column}. The
    // IR value model carries no attr position table, so this is a KNOWN
    // POTENTIAL DIVERGENCE (only byte-faithful if the position never flows into
    // hello.drvPath; the final byte-match arbitrates).
    UnsafeGetAttrPos,
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
    // arity 2 — slice 4
    Add,
    Sub,
    Mul,
    Div,
    LessThan,
    BitAnd,
    BitOr,
    BitXor,
    Elem,
    Sort,
    All,
    Any,
    Partition,
    GroupBy,
    ConcatMap,
    CatAttrs,
    ZipAttrsWith,
    CompareVersions,
    Match,
    FindFile,
    Trace,
    TraceVerbose,
    // arity 2 — slice 6 (curried, like the walker's `register_curried`)
    HashString,
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
            | B::Import
            | B::Throw
            | B::Abort
            | B::TryEval
            | B::Ceil
            | B::Floor
            | B::ToJson
            | B::FromJson
            | B::ToXml
            | B::FunctionArgs
            | B::GenericClosure
            | B::SplitVersion
            | B::ParseDrvName
            | B::HasContext
            | B::GetContext
            | B::UnsafeDiscardStringContext
            | B::BaseNameOf
            | B::DirOf
            | B::Derivation
            | B::DerivationStrict
            | B::ConvertHash
            | B::PathExists
            | B::ReadFile
            | B::ReadDir
            | B::ReadFileType
            | B::GetEnv
            | B::Placeholder => 1,
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
            | B::Split
            | B::Add
            | B::Sub
            | B::Mul
            | B::Div
            | B::LessThan
            | B::BitAnd
            | B::BitOr
            | B::BitXor
            | B::Elem
            | B::Sort
            | B::All
            | B::Any
            | B::Partition
            | B::GroupBy
            | B::ConcatMap
            | B::CatAttrs
            | B::ZipAttrsWith
            | B::CompareVersions
            | B::Match
            | B::FindFile
            | B::Trace
            | B::TraceVerbose
            | B::AddErrorContext
            | B::UnsafeGetAttrPos
            | B::HashString => 2,
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
            B::Throw => "throw",
            B::Abort => "abort",
            B::TryEval => "tryEval",
            B::Ceil => "ceil",
            B::Floor => "floor",
            B::ToJson => "toJSON",
            B::FromJson => "fromJSON",
            B::ToXml => "toXML",
            B::FunctionArgs => "functionArgs",
            B::GenericClosure => "genericClosure",
            B::SplitVersion => "splitVersion",
            B::ParseDrvName => "parseDrvName",
            B::HasContext => "hasContext",
            B::GetContext => "getContext",
            B::UnsafeDiscardStringContext => "unsafeDiscardStringContext",
            B::BaseNameOf => "baseNameOf",
            B::DirOf => "dirOf",
            B::Derivation => "derivation",
            B::DerivationStrict => "derivationStrict",
            B::ConvertHash => "convertHash",
            B::HashString => "hashString",
            B::PathExists => "pathExists",
            B::ReadFile => "readFile",
            B::ReadDir => "readDir",
            B::ReadFileType => "readFileType",
            B::GetEnv => "getEnv",
            B::Placeholder => "placeholder",
            B::AddErrorContext => "addErrorContext",
            B::UnsafeGetAttrPos => "unsafeGetAttrPos",
            B::Add => "add",
            B::Sub => "sub",
            B::Mul => "mul",
            B::Div => "div",
            B::LessThan => "lessThan",
            B::BitAnd => "bitAnd",
            B::BitOr => "bitOr",
            B::BitXor => "bitXor",
            B::Elem => "elem",
            B::Sort => "sort",
            B::All => "all",
            B::Any => "any",
            B::Partition => "partition",
            B::GroupBy => "groupBy",
            B::ConcatMap => "concatMap",
            B::CatAttrs => "catAttrs",
            B::ZipAttrsWith => "zipAttrsWith",
            B::CompareVersions => "compareVersions",
            B::Match => "match",
            B::FindFile => "findFile",
            B::Trace => "trace",
            B::TraceVerbose => "traceVerbose",
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
            // The `register_curried` family — arithmetic, regex `match`,
            // `findFile` — all share the anonymous `curried<partial>` stage.
            (
                B::Add
                | B::Sub
                | B::Mul
                | B::Div
                | B::LessThan
                | B::BitAnd
                | B::BitOr
                | B::BitXor
                | B::Match
                | B::FindFile
                | B::HashString,
                _,
            ) => "curried<partial>",
            // The `register_builtin` staged-closure family names its partial.
            (B::Elem, _) => "elem<partial>",
            (B::Sort, _) => "sort<partial>",
            (B::All, _) => "all<partial>",
            (B::Any, _) => "any<partial>",
            (B::Partition, _) => "partition<partial>",
            (B::GroupBy, _) => "groupBy<partial>",
            (B::ConcatMap, _) => "concatMap<partial>",
            (B::CatAttrs, _) => "catAttrs<partial>",
            (B::ZipAttrsWith, _) => "zipAttrsWith<partial>",
            (B::CompareVersions, _) => "compareVersions<partial>",
            (B::Trace, _) => "trace<partial>",
            (B::TraceVerbose, _) => "traceVerbose<partial>",
            (B::Foldl, 1) => "foldl'<p1>",
            (B::Foldl, _) => "foldl'<p2>",
            (B::Substring, 1) => "substring<p1>",
            (B::Substring, _) => "substring<p2>",
            (B::ReplaceStrings, 1) => "replaceStrings<p1>",
            (B::ReplaceStrings, _) => "replaceStrings<p2>",
            _ => self.registry_name(),
        }
    }

    /// Whether the NEXT argument must be passed **unforced**. Two cases
    /// mirror the walker: the `seq<partial>` / `deepSeq<partial>` stage
    /// returns its arg unforced (`apply_inner`), and `tryEval` MUST receive
    /// its argument unforced so it can force-and-catch itself (a
    /// pre-force in `apply` would raise the throw before `tryEval` runs).
    #[must_use]
    pub fn wants_unforced_arg(self, captured: usize) -> bool {
        (matches!(self, IrBuiltin::Seq | IrBuiltin::DeepSeq) && captured == 1)
            || (matches!(self, IrBuiltin::TryEval) && captured == 0)
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
    IrBuiltin::Throw,
    IrBuiltin::Abort,
    IrBuiltin::TryEval,
    IrBuiltin::Ceil,
    IrBuiltin::Floor,
    IrBuiltin::ToJson,
    IrBuiltin::FromJson,
    IrBuiltin::ToXml,
    IrBuiltin::FunctionArgs,
    IrBuiltin::GenericClosure,
    IrBuiltin::SplitVersion,
    IrBuiltin::ParseDrvName,
    IrBuiltin::HasContext,
    IrBuiltin::GetContext,
    IrBuiltin::UnsafeDiscardStringContext,
    IrBuiltin::BaseNameOf,
    IrBuiltin::DirOf,
    IrBuiltin::Derivation,
    IrBuiltin::DerivationStrict,
    IrBuiltin::ConvertHash,
    IrBuiltin::HashString,
    IrBuiltin::PathExists,
    IrBuiltin::ReadFile,
    IrBuiltin::ReadDir,
    IrBuiltin::ReadFileType,
    IrBuiltin::GetEnv,
    IrBuiltin::Placeholder,
    IrBuiltin::AddErrorContext,
    IrBuiltin::UnsafeGetAttrPos,
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
    IrBuiltin::Add,
    IrBuiltin::Sub,
    IrBuiltin::Mul,
    IrBuiltin::Div,
    IrBuiltin::LessThan,
    IrBuiltin::BitAnd,
    IrBuiltin::BitOr,
    IrBuiltin::BitXor,
    IrBuiltin::Elem,
    IrBuiltin::Sort,
    IrBuiltin::All,
    IrBuiltin::Any,
    IrBuiltin::Partition,
    IrBuiltin::GroupBy,
    IrBuiltin::ConcatMap,
    IrBuiltin::CatAttrs,
    IrBuiltin::ZipAttrsWith,
    IrBuiltin::CompareVersions,
    IrBuiltin::Match,
    IrBuiltin::FindFile,
    IrBuiltin::Trace,
    IrBuiltin::TraceVerbose,
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
    // Slice 4 implemented the pure surface; what remains is
    // store-/IO-/derivation-/flake-/crypto-bound (impure by nature) plus a
    // few pure-but-deferred context/passthrough helpers
    // (`addErrorContext`/`warn`/`break`/`appendContext`/…) left as typed
    // gaps for a later slice. `nixPath` moved OUT of this list — it is now a
    // real constant value (built from NIX_PATH), like the walker's.
    // Slice 6 implemented `derivation`/`derivationStrict`/`hashString`/
    // `convertHash`, so they moved OUT of this list into `ALL_IMPLEMENTED`.
    "addDrvOutputDependencies",
    "appendContext",
    "break",
    "currentTime",
    "fetchGit",
    "fetchMercurial",
    "fetchTarball",
    "fetchTree",
    "fetchurl",
    "filterSource",
    "flakeRefToString",
    "fromTOML",
    "getFlake",
    "hashFile",
    "parseFlakeRef",
    "path",
    "resolveFlakeRef",
    "scopedImport",
    "storePath",
    "sui",
    "toFile",
    "toPath",
    "unsafeDiscardOutputDependency",
    "warn",
];

/// Builtins CppNix exposes bare at top level.
///
/// This was a hand-written list whose doc-comment said "mirrored from
/// `sui-eval/src/builtins/mod.rs`" — and mirroring by hand is exactly how it
/// went wrong: both this copy and the walker's were missing `break`, which the
/// bytecode VM's copy had. `break` is a real nix global
/// (`builtins.typeOf break` → `lambda`, nix 2.31.5), so a `with` could shadow
/// it here and silently change what a program means.
///
/// Now derived from the single shared list. A comment claiming two lists match
/// is not a mechanism; this is.
fn default_scope() -> Vec<&'static str> {
    sui_compat::scope::CALLABLE_GLOBALS
        .iter()
        .copied()
        .chain(["true", "false", "null"])
        .collect()
}

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

/// `builtins.nixPath` — mirror of the walker's constant: `NIX_PATH` parsed
/// into a list of `{ prefix; path; }` attrsets (empty when unset).
#[must_use]
fn nix_path_value() -> IrValue {
    let raw = std::env::var("NIX_PATH").unwrap_or_default();
    let list: Vec<IrValue> = crate::path::parse_nix_path(&raw)
        .into_iter()
        .map(|(prefix, path)| {
            let mut a = IrAttrs::new();
            a.insert("prefix".to_string(), IrValue::string(prefix));
            a.insert("path".to_string(), IrValue::string(path));
            IrValue::Attrs(Rc::new(a))
        })
        .collect();
    IrValue::List(Rc::new(list))
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
    // Constants. This block used to be commented "mirroring the walker's values
    // byte-for-byte" — a claimed property that nothing enforced, and which the
    // BYTECODE VM had already violated (nixVersion "2.24.0" against the
    // walker's "2.34.7"). Mirroring by hand is what made three copies free to
    // disagree, so the version pair is now DERIVED from one typed source rather
    // than asserted in prose.
    set.insert("storeDir".to_string(), IrValue::string("/nix/store"));
    set.insert(
        "nixVersion".to_string(),
        IrValue::string(sui_compat::versions::IMPERSONATED_NIX_VERSION),
    );
    set.insert(
        "currentSystem".to_string(),
        IrValue::string(current_system()),
    );
    set.insert(
        "langVersion".to_string(),
        IrValue::Int(sui_compat::versions::LANG_VERSION),
    );
    set.insert("true".to_string(), IrValue::Bool(true));
    set.insert("false".to_string(), IrValue::Bool(false));
    set.insert("null".to_string(), IrValue::Null);
    // `builtins.nixPath` — a list of `{ prefix; path; }` built from NIX_PATH,
    // exactly like the walker's constant (both engines read the same env, so
    // the list is identical within a run). Underpins `__findFile __nixPath`.
    set.insert("nixPath".to_string(), nix_path_value());
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
    for name in &default_scope() {
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
        IrValue::Str(s, _) => Ok(s),
        other => Err(IrEvalError::TypeMismatch {
            expected: "string",
            got: other.type_name(),
        }),
    }
}

/// Coerce a value to a path string for the fs-reader builtins. Accepts a
/// `Path` (the common case) or a plain `Str` (the walker's `as_string`/
/// `coerce_to_realized_path` accept both; the flake `-source` redirect +
/// derivation IFD arms of the walker are NOT mirrored here — a probe gap
/// that only matters if a fetched flake input or an IFD path flows in).
fn as_path_string(v: &IrValue) -> Result<String, IrEvalError> {
    match v {
        IrValue::Path(p) => Ok((**p).clone()),
        IrValue::Str(s, _) => Ok((**s).clone()),
        other => Err(IrEvalError::TypeMismatch {
            expected: "path",
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
///
/// **Cycle-safe**, mirroring `sui_eval::builtins::control::deep_force` and
/// cppnix's `forceValueDeep` `std::set<const Value*> seen`.
///
/// This function used to recurse unconditionally, so
/// `let as = { x = 123; y = as; }; in builtins.deepSeq as 456` — the vendored
/// CppNix fixture `eval-okay-deepseq` — recursed forever and aborted the
/// process with a stack overflow. Not a hang: 256 MB of stack was exhausted
/// outright.
///
/// The tree-walker had the identical bug and it was fixed on 2026-07-22, with
/// the fixture graduated out of `known_broken/` as proof. **The fix never
/// reached this engine**, because nothing ran the corpus against it — the 117
/// lang fixtures had exactly one consumer, `sui-eval/tests/lang_corpus.rs`.
/// `sui-ir/tests/lang_corpus_ir.rs` is what found it, on its first run.
///
/// Never removing from `seen` is correct AND an optimization: a value shared
/// across two branches (a DAG, not a cycle) is deep-forced once, because
/// forcing the shared `Rc` once forces it everywhere. cppnix does the same.
fn deep_force(v: &IrValue) -> Result<(), IrEvalError> {
    let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
    deep_force_seen(v, &mut seen)
}

fn deep_force_seen(
    v: &IrValue,
    seen: &mut std::collections::HashSet<usize>,
) -> Result<(), IrEvalError> {
    let forced = v.force()?;
    match &forced {
        IrValue::Attrs(attrs) => {
            // Break cycles: an attrset already descended into is skipped.
            if !seen.insert(std::rc::Rc::as_ptr(attrs).cast::<()>() as usize) {
                return Ok(());
            }
            for value in attrs.values() {
                deep_force_seen(value, seen)?;
            }
        }
        IrValue::List(items) => {
            if !seen.insert(std::rc::Rc::as_ptr(items).cast::<()>() as usize) {
                return Ok(());
            }
            for item in items.iter() {
                deep_force_seen(item, seen)?;
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
        // String-first curried builtins. `catAttrs`/`compareVersions` validate
        // their first (string) argument AT CAPTURE (the walker's staged
        // closures run `as_string()?` there). `hasPrefix`/`hasSuffix` used to
        // be in this group; they were removed as nixpkgs `lib.strings` leaks.
        (
            B::HasAttr
            | B::GetAttr
            | B::ConcatStringsSep
            | B::CatAttrs
            | B::CompareVersions,
            0,
        ) => as_str(arg).map(|_| ()),
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
        B::IsString => Ok(IrValue::Bool(matches!(arg, IrValue::Str(..)))),
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
            // Mirror the walker: ACCUMULATE each element's string context into
            // the result (CppNix `prim_concatStringsSep` does `mkString(res,
            // context)`). Dropping it loses every input-drv edge a coerced
            // `${pkg.out}/lib` element carried — which diverges a consuming
            // derivation's drvPath (the `concatStringsSep multi-output context`
            // corpus row). PLAIN coercion (elements are already strings).
            let sep = as_str(&captured[0])?;
            let list = as_list(&arg)?;
            let mut result = String::new();
            let mut ctx = crate::eval_ir::IrStringContext::new();
            for (i, v) in list.iter().enumerate() {
                if i > 0 {
                    result.push_str(sep);
                }
                let (s, c) = crate::eval_ir::coerce_to_string_ctx(&v.force()?, false)?;
                result.push_str(&s);
                ctx.merge(&c);
            }
            Ok(IrValue::string_with_context(result, ctx))
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

        // ── slice 4: control ──────────────────────────────────────────────
        B::Throw => {
            let msg = as_str(&arg)?;
            Err(IrEvalError::Throw(format!("throw: {msg}")))
        }
        B::Abort => {
            let msg = as_str(&arg)?;
            Err(IrEvalError::Abort(format!(
                "evaluation aborted with the following error message: '{msg}'"
            )))
        }
        B::TryEval => {
            // The arg arrives UNFORCED (`wants_unforced_arg`); force it here
            // and catch exactly the walker's two catchable classes (`throw`
            // + `assert`), propagating everything else (abort included).
            match arg.force() {
                Ok(v) => Ok(try_eval_result(true, v)),
                Err(IrEvalError::Throw(_) | IrEvalError::AssertionFailed) => {
                    Ok(try_eval_result(false, IrValue::Bool(false)))
                }
                Err(e) => Err(e),
            }
        }
        // trace / traceVerbose: the message was forced at capture (WHNF via
        // `apply`); the eprintln side effect is irrelevant to value parity,
        // so this simply returns the second argument (the walker's
        // `<partial>` closure returns `args2[0]`).
        B::Trace | B::TraceVerbose => Ok(arg),

        // ── slice 4: arithmetic ───────────────────────────────────────────
        B::Add => numeric_binop(&captured[0], &arg, |a, b| a + b, |a, b| a + b, "add"),
        B::Sub => numeric_binop(&captured[0], &arg, |a, b| a - b, |a, b| a - b, "sub"),
        B::Mul => numeric_binop(&captured[0], &arg, |a, b| a * b, |a, b| a * b, "mul"),
        B::Div => div_impl(&captured[0], &arg),
        B::LessThan => less_than(&captured[0], &arg),
        B::BitAnd => Ok(IrValue::Int(as_int(&captured[0])? & as_int(&arg)?)),
        B::BitOr => Ok(IrValue::Int(as_int(&captured[0])? | as_int(&arg)?)),
        B::BitXor => Ok(IrValue::Int(as_int(&captured[0])? ^ as_int(&arg)?)),
        B::Ceil => Ok(IrValue::Int(to_float(&arg)?.ceil() as i64)),
        B::Floor => Ok(IrValue::Int(to_float(&arg)?.floor() as i64)),

        // ── slice 4: list HOFs ────────────────────────────────────────────
        B::Elem => {
            let needle = &captured[0];
            let list = as_list(&arg)?;
            Ok(IrValue::Bool(list.iter().any(|v| ir_eq(needle, v))))
        }
        B::Sort => sort_impl(&captured[0], &arg),
        B::All => {
            let pred = captured[0].clone();
            for v in as_list(&arg)?.iter() {
                if !apply(pred.clone(), v.clone())?.force()?.as_bool()? {
                    return Ok(IrValue::Bool(false));
                }
            }
            Ok(IrValue::Bool(true))
        }
        B::Any => {
            let pred = captured[0].clone();
            for v in as_list(&arg)?.iter() {
                if apply(pred.clone(), v.clone())?.force()?.as_bool()? {
                    return Ok(IrValue::Bool(true));
                }
            }
            Ok(IrValue::Bool(false))
        }
        B::Partition => {
            let pred = captured[0].clone();
            let mut right = Vec::new();
            let mut wrong = Vec::new();
            for v in as_list(&arg)?.iter() {
                if apply(pred.clone(), v.clone())?.force()?.as_bool()? {
                    right.push(v.clone());
                } else {
                    wrong.push(v.clone());
                }
            }
            let mut result = IrAttrs::new();
            result.insert("right".to_string(), IrValue::List(Rc::new(right)));
            result.insert("wrong".to_string(), IrValue::List(Rc::new(wrong)));
            Ok(IrValue::Attrs(Rc::new(result)))
        }
        B::GroupBy => {
            let func = captured[0].clone();
            let mut groups: std::collections::BTreeMap<String, Vec<IrValue>> =
                std::collections::BTreeMap::new();
            for v in as_list(&arg)?.iter() {
                let key = apply(func.clone(), v.clone())?.force()?;
                let key_str = as_str(&key)?.to_string();
                groups.entry(key_str).or_default().push(v.clone());
            }
            let mut result = IrAttrs::new();
            for (k, vs) in groups {
                result.insert(k, IrValue::List(Rc::new(vs)));
            }
            Ok(IrValue::Attrs(Rc::new(result)))
        }
        B::ConcatMap => {
            let func = captured[0].clone();
            let mut result = Vec::new();
            for v in as_list(&arg)?.iter() {
                let mapped = apply(func.clone(), v.clone())?.force()?;
                result.extend(as_list(&mapped)?.iter().cloned());
            }
            Ok(IrValue::List(Rc::new(result)))
        }

        // ── slice 4: attr HOFs ────────────────────────────────────────────
        B::CatAttrs => {
            let name = as_str(&captured[0])?;
            let mut result = Vec::new();
            for item in as_list(&arg)?.iter() {
                // Mirror the walker's `if let Ok(attrs) = item.to_attrs()`:
                // a non-attrs (or force-failing) element is silently skipped.
                if let Ok(forced) = item.force() {
                    if let IrValue::Attrs(a) = &forced {
                        if let Some(v) = a.get(name) {
                            result.push(v.clone());
                        }
                    }
                }
            }
            Ok(IrValue::List(Rc::new(result)))
        }
        B::ZipAttrsWith => {
            let func = captured[0].clone();
            let mut collected: std::collections::BTreeMap<String, Vec<IrValue>> =
                std::collections::BTreeMap::new();
            for item in as_list(&arg)?.iter() {
                let forced = item.force()?;
                for (k, v) in as_attrs(&forced)?.iter() {
                    collected.entry(k.clone()).or_default().push(v.clone());
                }
            }
            let mut result = IrAttrs::new();
            for (k, vs) in collected {
                // Lazy `f key values` thunk per key — mirrors the walker.
                let thunk = IrThunk::native_apply(
                    func.clone(),
                    vec![IrValue::string(k.clone()), IrValue::List(Rc::new(vs))],
                );
                result.insert(k, IrValue::Thunk(thunk));
            }
            Ok(IrValue::Attrs(Rc::new(result)))
        }
        B::FunctionArgs => function_args(&arg),
        B::GenericClosure => generic_closure(&arg),

        // ── slice 4: strings + versions ───────────────────────────────────
        B::Match => match_impl(as_str(&captured[0])?, as_str(&arg)?),
        B::CompareVersions => {
            let a = as_str(&captured[0])?;
            let b = as_str(&arg)?;
            Ok(IrValue::Int(compare_versions(a, b)))
        }
        B::SplitVersion => {
            let s = as_str(&arg)?;
            Ok(IrValue::List(Rc::new(
                split_version(s).into_iter().map(IrValue::string).collect(),
            )))
        }
        B::ParseDrvName => {
            let s = as_str(&arg)?;
            let (name, version) = parse_drv_name(s);
            let mut result = IrAttrs::new();
            result.insert("name".to_string(), IrValue::string(name));
            result.insert("version".to_string(), IrValue::string(version));
            Ok(IrValue::Attrs(Rc::new(result)))
        }

        // ── slice 6: context — wired to REAL string context ───────────────
        // Since derivations now produce context-bearing `.drvPath`/`.outPath`
        // strings, `${pkg}`-derived strings carry context and these builtins
        // report it, mirroring the walker's `context.rs` byte-for-byte.
        B::HasContext => match &arg {
            IrValue::Str(_, c) => Ok(IrValue::Bool(c.is_some())),
            other => Err(IrEvalError::TypeError(format!(
                "hasContext: expected string, got {}",
                other.type_name()
            ))),
        },
        B::GetContext => match &arg {
            // Mirror of the walker's `getContext`: group the context elements
            // into `{ "<path>" = { path = true; }; "<drv>" = { outputs = [...]; };
            // "<drv>" = { allOutputs = true; }; }`. BTreeMap/BTreeSet give the
            // walker's sorted-attrset iteration order by construction.
            IrValue::Str(_, c) => {
                use std::collections::{BTreeMap, BTreeSet};
                let mut plains: BTreeSet<String> = BTreeSet::new();
                let mut om: BTreeMap<String, Vec<String>> = BTreeMap::new();
                let mut deep: BTreeSet<String> = BTreeSet::new();
                if let Some(c) = c {
                    for elem in c.iter() {
                        match elem {
                            IrContextElem::Plain(p) => {
                                plains.insert(p.clone());
                            }
                            IrContextElem::Output { drv, output } => {
                                om.entry(drv.clone()).or_default().push(output.clone());
                            }
                            IrContextElem::DrvDeep(d) => {
                                deep.insert(d.clone());
                            }
                        }
                    }
                }
                let mut result = IrAttrs::new();
                for p in &plains {
                    let mut a = IrAttrs::new();
                    a.insert("path".to_string(), IrValue::Bool(true));
                    result.insert(p.clone(), IrValue::Attrs(Rc::new(a)));
                }
                for (d, os) in &om {
                    let mut a = IrAttrs::new();
                    a.insert(
                        "outputs".to_string(),
                        IrValue::List(Rc::new(
                            os.iter().map(|o| IrValue::string(o.clone())).collect(),
                        )),
                    );
                    result.insert(d.clone(), IrValue::Attrs(Rc::new(a)));
                }
                for d in &deep {
                    let mut a = IrAttrs::new();
                    a.insert("allOutputs".to_string(), IrValue::Bool(true));
                    result.insert(d.clone(), IrValue::Attrs(Rc::new(a)));
                }
                Ok(IrValue::Attrs(Rc::new(result)))
            }
            other => Err(IrEvalError::TypeError(format!(
                "getContext: expected string, got {}",
                other.type_name()
            ))),
        },
        B::UnsafeDiscardStringContext => match &arg {
            // Strip the context, keep the chars (the walker's identity-on-chars).
            IrValue::Str(s, _) => Ok(IrValue::Str(s.clone(), None)),
            other => Err(IrEvalError::TypeError(format!(
                "unsafeDiscardStringContext: expected string, got {}",
                other.type_name()
            ))),
        },

        // ── slice 6: derivation + crypto ──────────────────────────────────
        // `derivation` / `derivationStrict` route to the shared spec
        // interpreter (byte-identical drvPath to the walker + nix).
        B::Derivation | B::DerivationStrict => crate::derivation::build_derivation(&arg),
        // `hashString "<algo>" "<str>"` → lowercase-hex digest, mirroring the
        // walker's md5/sha1/sha256/sha512 (same crates, same bytes).
        B::HashString => hash_string(as_str(&captured[0])?, as_str(&arg)?),
        // `convertHash { hash; hashAlgo?; toHashFormat; }` — decode a hash and
        // re-encode it; mirror of the walker's `convertHash`.
        B::ConvertHash => convert_hash(as_attrs(&arg)?),

        // ── slice 4: paths (string ops) + search path ─────────────────────
        B::BaseNameOf => match &arg {
            IrValue::Str(s, _) => Ok(IrValue::string(base_name_of(s))),
            IrValue::Path(p) => Ok(IrValue::string(base_name_of(p))),
            other => Err(IrEvalError::TypeError(format!(
                "baseNameOf: expected string or path, got {}",
                other.type_name()
            ))),
        },
        B::DirOf => match &arg {
            IrValue::Str(s, _) => Ok(IrValue::string(dir_of(s))),
            IrValue::Path(p) => Ok(IrValue::Path(Rc::new(dir_of(p)))),
            other => Err(IrEvalError::TypeError(format!(
                "dirOf: expected string or path, got {}",
                other.type_name()
            ))),
        },
        B::FindFile => find_file_impl(as_list(&captured[0])?, as_str(&arg)?),

        // ── probe: pure-fs readers (mirror the walker's paths.rs) ─────────
        B::PathExists => {
            let p = as_path_string(&arg)?;
            Ok(IrValue::Bool(std::path::Path::new(&p).exists()))
        }
        B::ReadFile => {
            let p = as_path_string(&arg)?;
            let contents = std::fs::read_to_string(&p).map_err(|e| IrEvalError::Io {
                context: "readFile".to_string(),
                message: e.to_string(),
            })?;
            Ok(IrValue::string(contents))
        }
        B::ReadFileType => {
            let p = as_path_string(&arg)?;
            match std::fs::symlink_metadata(&p) {
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
                    Ok(IrValue::string(kind))
                }
                Err(e) => Err(IrEvalError::Io {
                    context: "readFileType".to_string(),
                    message: e.to_string(),
                }),
            }
        }
        B::ReadDir => {
            let p = as_path_string(&arg)?;
            let mut attrs = IrAttrs::new();
            for entry in std::fs::read_dir(&p).map_err(|e| IrEvalError::Io {
                context: "readDir".to_string(),
                message: e.to_string(),
            })? {
                let entry = entry.map_err(|e| IrEvalError::Io {
                    context: "readDir".to_string(),
                    message: e.to_string(),
                })?;
                let name = entry.file_name().to_string_lossy().to_string();
                let ft = entry.file_type().map_err(|e| IrEvalError::Io {
                    context: "readDir".to_string(),
                    message: e.to_string(),
                })?;
                let type_str = if ft.is_dir() {
                    "directory"
                } else if ft.is_symlink() {
                    "symlink"
                } else {
                    "regular"
                };
                attrs.insert(name, IrValue::string(type_str));
            }
            Ok(IrValue::Attrs(Rc::new(attrs)))
        }

        B::GetEnv => {
            // mirror of the walker: `std::env::var(name).unwrap_or_default()`
            let name = as_str(&arg)?;
            Ok(IrValue::string(std::env::var(name).unwrap_or_default()))
        }
        // `placeholder name` = "/" + nix_base32(sha256("nix-output:"+name)) —
        // pure, byte-exact (embedded verbatim in self-referencing drv env/args).
        B::Placeholder => {
            let output = as_str(&arg)?;
            use sha2::{Digest, Sha256};
            let hash = Sha256::digest(format!("nix-output:{output}").as_bytes());
            Ok(IrValue::string(format!(
                "/{}",
                sui_compat::store_path::nix_base32_encode(hash.as_slice())
            )))
        }
        // `addErrorContext ctx value` → value (identity on the 2nd arg; the
        // context only decorates errors thrown while forcing — value-parity
        // is identity). Mirror of the walker's curried passthrough.
        B::AddErrorContext => Ok(arg),
        // TYPED KNOWN GAP (the position-less IR value model). CppNix (and the
        // walker) return the attr's real `{file,line,column}` for a
        // SOURCE-LITERAL attr, and `null` for a POSITIONLESS attr (one
        // synthesized by `//` / `listToAttrs` / `mapAttrs` — the common case in
        // generic stdenv/builder logic). The IR's `IrAttrs` (a `BTreeMap`)
        // carries no source positions at all, so the honest floor is `null` for
        // EVERY attr: byte-correct for the positionless case (which is what the
        // deep nixpkgs fixpoint actually exercises), a NAMED divergence for the
        // source-literal case. It never affects a drvPath (a position byte never
        // enters derivation hashing), so a real hello.drvPath is reachable
        // through this gap; the source-literal divergence is asserted (not
        // silent) in `eval_ir::tests::typed_gaps`. Closing it truly-unrep needs
        // IR attr-position tracking through lower+eval (a later slice).
        B::UnsafeGetAttrPos => Ok(IrValue::Null),

        // ── slice 4: convert ──────────────────────────────────────────────
        B::ToJson => {
            let json = ir_to_json(&arg)?;
            let s = serde_json::to_string(&json).unwrap_or_else(|_| "null".to_string());
            Ok(IrValue::string(s))
        }
        B::FromJson => {
            let s = as_str(&arg)?;
            let json: serde_json::Value = serde_json::from_str(s)
                .map_err(|e| IrEvalError::TypeError(format!("fromJSON: {e}")))?;
            Ok(json_to_ir(&json))
        }
        B::ToXml => Ok(IrValue::string(to_xml(&arg)?)),
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

/// `builtins.hashString "<algo>" "<str>"` → lowercase-hex digest. Byte-mirror
/// of the walker's `hashString` (`sui_eval::builtins::strings`): the SAME
/// md5/sha1/sha256/sha512 crates, the same `{:x}` hex formatting, so the digest
/// bytes cannot drift.
fn hash_string(algo: &str, input: &str) -> Result<IrValue, IrEvalError> {
    let hex = match algo {
        "md5" => {
            use md5::{Digest, Md5};
            format!("{:x}", Md5::digest(input.as_bytes()))
        }
        "sha1" => {
            use sha1::{Digest, Sha1};
            format!("{:x}", Sha1::digest(input.as_bytes()))
        }
        "sha256" => {
            use sha2::{Digest, Sha256};
            format!("{:x}", Sha256::digest(input.as_bytes()))
        }
        "sha512" => {
            use sha2::{Digest, Sha512};
            format!("{:x}", Sha512::digest(input.as_bytes()))
        }
        other => {
            return Err(IrEvalError::TypeError(format!(
                "hashString: unsupported algorithm: {other}"
            )))
        }
    };
    Ok(IrValue::string(hex))
}

/// `builtins.convertHash { hash; hashAlgo?; toHashFormat; }` — decode a hash
/// from hex / nix32 / base64 and re-encode to base16 / nix32 / base64 / sri.
/// Byte-mirror of the walker's `convertHash` (`sui_eval::builtins::convert`),
/// sharing `sui_compat`'s base64 (STANDARD) + nix-base32 codecs so the output
/// bytes cannot drift.
fn convert_hash(attrs: &IrAttrs) -> Result<IrValue, IrEvalError> {
    let hash_str = as_str(&attr_force(attrs, "hash")?)?.to_string();
    let to_format = as_str(&attr_force(attrs, "toHashFormat")?)?.to_string();

    let (algo, raw_hash): (String, String) = if let Some(av) = attrs.get("hashAlgo") {
        (as_str(&av.force()?)?.to_string(), hash_str.clone())
    } else if let Some(stripped) = hash_str.strip_prefix("sha256-") {
        ("sha256".to_string(), stripped.to_string())
    } else if let Some(stripped) = hash_str.strip_prefix("sha512-") {
        ("sha512".to_string(), stripped.to_string())
    } else {
        return Err(IrEvalError::TypeError("convertHash: missing hashAlgo".into()));
    };

    let expected_len = match algo.as_str() {
        "md5" => 16,
        "sha1" => 20,
        "sha256" => 32,
        "sha512" => 64,
        other => {
            return Err(IrEvalError::TypeError(format!(
                "convertHash: unsupported algo {other}"
            )))
        }
    };

    let bytes: Vec<u8> = if raw_hash.len() == expected_len * 2
        && raw_hash.chars().all(|c| c.is_ascii_hexdigit())
    {
        (0..raw_hash.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&raw_hash[i..i + 2], 16))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| IrEvalError::TypeError(format!("convertHash hex: {e}")))?
    } else if let Ok(b) = sui_compat::store_path::nix_base32_decode(&raw_hash) {
        if expected_len != 20 {
            return Err(IrEvalError::TypeError(
                "convertHash: nix32 only supported for 20-byte (sha1) hashes".into(),
            ));
        }
        b.to_vec()
    } else if let Ok(b) = sui_compat::hash::base64_decode(&raw_hash) {
        b
    } else {
        return Err(IrEvalError::TypeError(format!(
            "convertHash: cannot decode hash '{raw_hash}'"
        )));
    };
    if bytes.len() != expected_len {
        return Err(IrEvalError::TypeError(format!(
            "convertHash: decoded {} bytes, expected {expected_len} for {algo}",
            bytes.len()
        )));
    }

    let out = match to_format.as_str() {
        "base16" => {
            let mut s = String::with_capacity(bytes.len() * 2);
            for b in &bytes {
                s.push_str(&format!("{b:02x}"));
            }
            s
        }
        "nix32" => {
            if expected_len != 20 {
                return Err(IrEvalError::TypeError(
                    "convertHash: nix32 output only supported for 20-byte hashes".into(),
                ));
            }
            sui_compat::store_path::nix_base32_encode(&bytes)
        }
        "base64" => sui_compat::hash::base64_encode(&bytes),
        "sri" => format!("{algo}-{}", sui_compat::hash::base64_encode(&bytes)),
        other => {
            return Err(IrEvalError::TypeError(format!(
                "convertHash: unsupported toHashFormat {other}"
            )))
        }
    };
    Ok(IrValue::string(out))
}

/// Force a required attr (the walker forces `convertHash`'s attrs strictly).
fn attr_force(attrs: &IrAttrs, key: &str) -> Result<IrValue, IrEvalError> {
    attrs
        .get(key)
        .ok_or_else(|| IrEvalError::AttrNotFound(key.to_string()))?
        .force()
}

/// The walker's `coerce_to_realized_path` restricted to the pure subset:
/// path and string values pass through; an attrset follows `outPath`
/// (derivation *realization* is unreachable here — no `derivation`).
fn coerce_import_path(v: &IrValue) -> Result<String, IrEvalError> {
    match v {
        IrValue::Path(p) => Ok((**p).clone()),
        IrValue::Str(s, _) => Ok((**s).clone()),
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

// ── slice 4 helpers ───────────────────────────────────────────────────────

/// The `{ success; value; }` attrset `tryEval` returns (BTreeMap key order
/// is `success` < `value`, matching the walker's `NixAttrs` render order).
fn try_eval_result(success: bool, value: IrValue) -> IrValue {
    let mut result = IrAttrs::new();
    result.insert("success".to_string(), IrValue::Bool(success));
    result.insert("value".to_string(), value);
    IrValue::Attrs(Rc::new(result))
}

/// Force to WHNF and require a number (the walker's `to_float`, used by
/// `ceil`/`floor`). Args here arrive already WHNF-forced, so no thunk chase.
fn to_float(v: &IrValue) -> Result<f64, IrEvalError> {
    match v {
        IrValue::Int(n) => Ok(*n as f64),
        IrValue::Float(f) => Ok(*f),
        other => Err(IrEvalError::TypeMismatch {
            expected: "number",
            got: other.type_name(),
        }),
    }
}

/// The walker's `register_numeric_binop!` shape for `add`/`sub`/`mul` —
/// Int+Int / Float+Float / the two mixed Int↔Float cases; anything else is a
/// type error. Uses plain arithmetic (no overflow trap), byte-mirroring the
/// walker's `builtins.add` (distinct from the `+` OPERATOR, which DOES trap).
fn numeric_binop(
    a: &IrValue,
    b: &IrValue,
    int_op: fn(i64, i64) -> i64,
    float_op: fn(f64, f64) -> f64,
    name: &str,
) -> Result<IrValue, IrEvalError> {
    match (a, b) {
        (IrValue::Int(x), IrValue::Int(y)) => Ok(IrValue::Int(int_op(*x, *y))),
        (IrValue::Float(x), IrValue::Float(y)) => Ok(IrValue::Float(float_op(*x, *y))),
        (IrValue::Int(x), IrValue::Float(y)) => Ok(IrValue::Float(float_op(*x as f64, *y))),
        (IrValue::Float(x), IrValue::Int(y)) => Ok(IrValue::Float(float_op(*x, *y as f64))),
        _ => Err(IrEvalError::TypeError(format!("{name}: expected numbers"))),
    }
}

/// `builtins.div` — integer division traps on `/0`; float division is IEEE
/// (inf/NaN), mirroring the walker.
fn div_impl(a: &IrValue, b: &IrValue) -> Result<IrValue, IrEvalError> {
    match (a, b) {
        (IrValue::Int(x), IrValue::Int(y)) => {
            if *y == 0 {
                return Err(IrEvalError::DivisionByZero);
            }
            Ok(IrValue::Int(x / y))
        }
        (IrValue::Float(x), IrValue::Float(y)) => Ok(IrValue::Float(x / y)),
        (IrValue::Int(x), IrValue::Float(y)) => Ok(IrValue::Float(*x as f64 / *y)),
        (IrValue::Float(x), IrValue::Int(y)) => Ok(IrValue::Float(*x / *y as f64)),
        _ => Err(IrEvalError::TypeError("div: expected numbers".to_string())),
    }
}

/// `builtins.lessThan` — numeric (with Int↔Float cross-compare) + string
/// (lexicographic), mirroring the walker.
fn less_than(a: &IrValue, b: &IrValue) -> Result<IrValue, IrEvalError> {
    let r = match (a, b) {
        (IrValue::Int(x), IrValue::Int(y)) => x < y,
        (IrValue::Float(x), IrValue::Float(y)) => x < y,
        (IrValue::Int(x), IrValue::Float(y)) => (*x as f64) < *y,
        (IrValue::Float(x), IrValue::Int(y)) => *x < (*y as f64),
        (IrValue::Str(x, _), IrValue::Str(y, _)) => x < y,
        _ => {
            return Err(IrEvalError::TypeError(
                "lessThan: expected comparable types".to_string(),
            ))
        }
    };
    Ok(IrValue::Bool(r))
}

/// `builtins.sort cmp list` — Rust's stable `sort_by` driven by the Nix
/// comparator (`cmp a b` true ⇒ `a` before `b`), a captured comparator error
/// propagated after the sort. Byte-mirrors the walker's `sort` exactly
/// (same stable sort, same error-capture), so the oracle is the gate.
fn sort_impl(cmp: &IrValue, arg: &IrValue) -> Result<IrValue, IrEvalError> {
    let cmp = cmp.clone();
    let mut list = as_list(arg)?.to_vec();
    if list.len() <= 1 {
        return Ok(IrValue::List(Rc::new(list)));
    }
    let mut err: Option<IrEvalError> = None;
    list.sort_by(|a, b| {
        if err.is_some() {
            return std::cmp::Ordering::Equal;
        }
        match apply(cmp.clone(), a.clone())
            .and_then(|partial| apply(partial, b.clone()))
            .and_then(|v| v.force())
            .and_then(|v| {
                v.as_bool().map_err(|_| {
                    IrEvalError::TypeError("sort comparator must return bool".to_string())
                })
            }) {
            Ok(true) => std::cmp::Ordering::Less,
            Ok(false) => std::cmp::Ordering::Greater,
            Err(e) => {
                err = Some(e);
                std::cmp::Ordering::Equal
            }
        }
    });
    if let Some(e) = err {
        return Err(e);
    }
    Ok(IrValue::List(Rc::new(list)))
}

/// `builtins.functionArgs` — for a lambda with a pattern param, `{ name =
/// hasDefault; … }`; a simple `x:` param and any builtin yield `{ }`. Mirrors
/// the walker's `misc::functionArgs`.
fn function_args(v: &IrValue) -> Result<IrValue, IrEvalError> {
    match v {
        IrValue::Lambda(closure) => {
            let mut result = IrAttrs::new();
            if let crate::ir::Param::Pattern { entries, .. } = &closure.param {
                for entry in entries {
                    result.insert(
                        sui_intern::resolve(entry.name),
                        IrValue::Bool(entry.default.is_some()),
                    );
                }
            }
            Ok(IrValue::Attrs(Rc::new(result)))
        }
        IrValue::Builtin(..) => Ok(IrValue::Attrs(Rc::new(IrAttrs::new()))),
        other => Err(IrEvalError::TypeError(format!(
            "functionArgs: expected function, got {}",
            other.type_name()
        ))),
    }
}

/// `builtins.genericClosure { startSet; operator; }` — BFS from `startSet`,
/// deduping by the `{ }`-Display of each item's `key`, applying `operator`
/// to expand. Mirrors the walker's `misc::genericClosure` (same VecDeque
/// work-list, same `format!("{}", key)` dedup via [`display_ir_value`]).
fn generic_closure(arg: &IrValue) -> Result<IrValue, IrEvalError> {
    use std::collections::{BTreeSet, VecDeque};
    let input = as_attrs(arg)?;
    let start = input
        .get("startSet")
        .ok_or_else(|| IrEvalError::AttrNotFound("startSet".to_string()))?
        .force()?;
    let start_set = as_list(&start)?.to_vec();
    let operator = input
        .get("operator")
        .ok_or_else(|| IrEvalError::AttrNotFound("operator".to_string()))?
        .clone();

    let mut result: Vec<IrValue> = Vec::new();
    let mut work_list: VecDeque<IrValue> = start_set.into();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    while let Some(item) = work_list.pop_front() {
        let item_forced = item.force()?;
        let item_attrs = as_attrs(&item_forced)?;
        let key_val = item_attrs
            .get("key")
            .ok_or_else(|| IrEvalError::AttrNotFound("key".to_string()))?
            .clone();
        let key_str = display_ir_value(&key_val.force()?);
        if seen.contains(&key_str) {
            continue;
        }
        seen.insert(key_str);
        result.push(item.clone());
        let new_items = apply(operator.clone(), item)?.force()?;
        work_list.extend(as_list(&new_items)?.iter().cloned());
    }
    Ok(IrValue::List(Rc::new(result)))
}

/// The walker's `Value` `Display`, mirrored for the pure subset — the exact
/// string form `genericClosure`'s dedup keys on (`format!("{}", value)`).
fn display_ir_value(v: &IrValue) -> String {
    match v {
        IrValue::Null => "null".to_string(),
        IrValue::Bool(b) => b.to_string(),
        IrValue::Int(n) => n.to_string(),
        IrValue::Float(f) => cppnix_format_float(*f),
        IrValue::Str(s, _) => {
            let mut out = String::from("\"");
            out.push_str(&s.replace('\\', "\\\\").replace('"', "\\\""));
            out.push('"');
            out
        }
        IrValue::Path(p) => (**p).clone(),
        IrValue::List(items) => {
            let mut out = String::from("[ ");
            for item in items.iter() {
                out.push_str(&display_forced(item));
                out.push(' ');
            }
            out.push(']');
            out
        }
        IrValue::Attrs(attrs) => {
            let mut out = String::from("{ ");
            for (k, val) in attrs.iter() {
                out.push_str(k);
                out.push_str(" = ");
                out.push_str(&display_forced(val));
                out.push_str("; ");
            }
            out.push('}');
            out
        }
        IrValue::Lambda(_) => "<<lambda>>".to_string(),
        IrValue::Builtin(kind, captured) => {
            let mut out = String::from("<<builtin ");
            out.push_str(kind.display_name(captured.len()));
            out.push_str(">>");
            out
        }
        IrValue::Thunk(_) => display_forced(v),
    }
}

/// Force `v` for `Display`, mirroring the walker's `Thunk` Display arm
/// (`<<thunk:error>>` on a failed force).
fn display_forced(v: &IrValue) -> String {
    match v.force() {
        Ok(f) => display_ir_value(&f),
        Err(_) => "<<thunk:error>>".to_string(),
    }
}

/// `builtins.match pattern s` — full-anchored (`^…$`) regex, returning the
/// capture-group list on a full match or `null` otherwise. Mirrors the
/// walker's `strings::match`.
fn match_impl(pattern: &str, input: &str) -> Result<IrValue, IrEvalError> {
    let anchored = format!("^{pattern}$");
    let re = cached_regex(&anchored)?;
    match re.captures(input) {
        Some(caps) => {
            let groups: Vec<IrValue> = (1..caps.len())
                .map(|i| match caps.get(i) {
                    Some(m) => IrValue::string(m.as_str().to_string()),
                    None => IrValue::Null,
                })
                .collect();
            Ok(IrValue::List(Rc::new(groups)))
        }
        None => Ok(IrValue::Null),
    }
}

/// `builtins.findFile searchPath name` — walk `{ prefix; path; }` entries,
/// return the first existing `path`+suffix as a `Path`, else a type error.
/// Mirrors the walker's `misc::findFile`.
fn find_file_impl(entries: &[IrValue], name: &str) -> Result<IrValue, IrEvalError> {
    for entry in entries {
        let forced = entry.force()?;
        let attrs = as_attrs(&forced)?;
        let prefix = force_str(
            attrs
                .get("prefix")
                .ok_or_else(|| IrEvalError::AttrNotFound("prefix".to_string()))?,
        )?;
        let path = force_str(
            attrs
                .get("path")
                .ok_or_else(|| IrEvalError::AttrNotFound("path".to_string()))?,
        )?;
        if name == prefix || name.starts_with(&format!("{prefix}/")) {
            let suffix = if name == prefix {
                String::new()
            } else {
                name[prefix.len()..].to_string()
            };
            let full_path = format!("{path}{suffix}");
            if std::path::Path::new(&full_path).exists() {
                return Ok(IrValue::Path(Rc::new(full_path)));
            }
        }
    }
    Err(IrEvalError::TypeError(format!(
        "findFile: file '{name}' not found in search path"
    )))
}

/// CppNix `baseNameOf`: strip trailing `/`, take the last component. Mirror
/// of the walker's `paths::base_name_of`.
fn base_name_of(s: &str) -> String {
    let trimmed = s.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
}

/// CppNix `dirOf`: everything up to the last `/` (root → `/`, no slash →
/// `.`). Mirror of the walker's `dirOf` component logic.
fn dir_of(s: &str) -> String {
    match s.rfind('/') {
        Some(0) => "/".to_string(),
        Some(i) => s[..i].to_string(),
        None => ".".to_string(),
    }
}

// ── toJSON / fromJSON (serde_json, mirroring the walker) ──────────────────

/// Mirror of the walker's `Value::to_json_with_context` restricted to the
/// pure subset: force each node, error on lambda/builtin, treat an attrset
/// carrying `__toString`/`outPath` as its coerced string. A `Path` is a
/// copy-to-store reach → a typed gap here.
fn ir_to_json(v: &IrValue) -> Result<serde_json::Value, IrEvalError> {
    let forced = v.force()?;
    Ok(match &forced {
        IrValue::Null => serde_json::Value::Null,
        IrValue::Bool(b) => serde_json::Value::Bool(*b),
        IrValue::Int(n) => serde_json::json!(*n),
        IrValue::Float(f) => serde_json::json!(*f),
        IrValue::Str(s, _) => serde_json::Value::String((**s).clone()),
        IrValue::Path(_) => return Err(IrEvalError::Unsupported("path-copy-to-store")),
        IrValue::List(items) => {
            let mut arr = Vec::with_capacity(items.len());
            for item in items.iter() {
                arr.push(ir_to_json(item)?);
            }
            serde_json::Value::Array(arr)
        }
        IrValue::Attrs(attrs) => {
            if attrs.contains_key("__toString") || attrs.contains_key("outPath") {
                return Ok(serde_json::Value::String(coerce_to_string_plain(&forced)?));
            }
            let mut map = serde_json::Map::new();
            for (k, val) in attrs.iter() {
                map.insert(k.clone(), ir_to_json(val)?);
            }
            serde_json::Value::Object(map)
        }
        IrValue::Lambda(_) | IrValue::Builtin(..) => {
            return Err(IrEvalError::TypeError(format!(
                "cannot serialize {} to JSON",
                forced.type_name()
            )));
        }
        IrValue::Thunk(_) => unreachable!("force() returned a thunk"),
    })
}

/// Mirror of the walker's `From<&serde_json::Value> for Value`.
fn json_to_ir(json: &serde_json::Value) -> IrValue {
    match json {
        serde_json::Value::Null => IrValue::Null,
        serde_json::Value::Bool(b) => IrValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                IrValue::Int(i)
            } else {
                IrValue::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => IrValue::string(s.clone()),
        serde_json::Value::Array(arr) => {
            IrValue::List(Rc::new(arr.iter().map(json_to_ir).collect()))
        }
        serde_json::Value::Object(obj) => {
            let mut attrs = IrAttrs::new();
            for (k, v) in obj {
                attrs.insert(k.clone(), json_to_ir(v));
            }
            IrValue::Attrs(Rc::new(attrs))
        }
    }
}

// ── toXML (mirroring the walker's `convert::toXML` byte-for-byte) ─────────
//
// LOCKSTEP: `sui-ir/tests/eval_differential.rs` compares this against the
// tree-walker's `builtins/convert.rs`, so the two move together or CI goes
// red. The escape table and the float formatter are NOT mirrored — they are
// shared outright via `sui_compat::versions`, because a re-derived
// cross-engine fact is exactly how these two drifted (both were missing the
// newline escape, identically and therefore invisibly).

use sui_compat::versions::xml_escape;

/// CppNix has no cycle protection in `printValueAsXML` — it SIGSEGVs. sui
/// refuses with a typed error instead; see the walker's `enter_cycle_guard`
/// for why this is an ANCESTOR stack and not a seen-set (CppNix re-expands
/// shared non-cyclic values, so a seen-set would break DAG parity).
fn enter_cycle_guard(ancestors: &mut Vec<usize>, key: usize) -> Result<(), IrEvalError> {
    if ancestors.contains(&key) {
        return Err(IrEvalError::Unsupported(
            "toXML: value contains a cycle",
        ));
    }
    ancestors.push(key);
    Ok(())
}

/// One XML node, byte-for-byte as CppNix's `printValueAsXML` writes it.
///
/// Forces at every node — `builtins.toXML` is `strict = true`, so `<thunk />`
/// (an element CppNix cannot produce) must never be reachable. The previous
/// version pattern-matched `IrValue::Thunk` and emitted exactly that.
fn value_to_xml(
    v: &IrValue,
    indent: usize,
    ancestors: &mut Vec<usize>,
) -> Result<String, IrEvalError> {
    let pad = " ".repeat(indent);
    Ok(match v.force()? {
        IrValue::Null => format!("{pad}<null />"),
        IrValue::Bool(b) => format!("{pad}<bool value=\"{b}\" />"),
        IrValue::Int(n) => format!("{pad}<int value=\"{n}\" />"),
        IrValue::Float(f) => {
            format!("{pad}<float value=\"{}\" />", cppnix_format_float(f))
        }
        IrValue::Str(s, _) => format!("{pad}<string value=\"{}\" />", xml_escape(&s)),
        IrValue::Path(p) => format!("{pad}<path value=\"{}\" />", xml_escape(&p)),
        IrValue::List(items) => {
            enter_cycle_guard(ancestors, Rc::as_ptr(&items) as usize)?;
            let mut out = format!("{pad}<list>\n");
            for item in items.iter() {
                out.push_str(&value_to_xml(item, indent + 2, ancestors)?);
                out.push('\n');
            }
            out.push_str(&format!("{pad}</list>"));
            ancestors.pop();
            out
        }
        IrValue::Attrs(attrs) => {
            enter_cycle_guard(ancestors, Rc::as_ptr(&attrs) as usize)?;
            let mut out = format!("{pad}<attrs>\n");
            for (k, val) in attrs.iter() {
                out.push_str(&format!("{pad}  <attr name=\"{}\">\n", xml_escape(k)));
                out.push_str(&value_to_xml(val, indent + 4, ancestors)?);
                out.push('\n');
                out.push_str(&format!("{pad}  </attr>\n"));
            }
            out.push_str(&format!("{pad}</attrs>"));
            ancestors.pop();
            out
        }
        // CppNix distinguishes three function shapes; a bare `<function />`
        // discarded the parameter names. The IR keeps them on `Param`.
        IrValue::Lambda(cl) => {
            let inner = match &cl.param {
                crate::ir::Param::Ident(sym) => format!(
                    "{pad}  <varpat name=\"{}\" />\n",
                    xml_escape(&sui_intern::resolve(*sym))
                ),
                crate::ir::Param::Pattern {
                    entries,
                    ellipsis,
                    bind,
                } => {
                    // CppNix writes `ellipsis` BEFORE `name`.
                    let ell = if *ellipsis { " ellipsis=\"1\"" } else { "" };
                    let name = bind
                        .map(|b| {
                            format!(" name=\"{}\"", xml_escape(&sui_intern::resolve(b)))
                        })
                        .unwrap_or_default();
                    let mut s = format!("{pad}  <attrspat{ell}{name}>\n");
                    for e in entries {
                        s.push_str(&format!(
                            "{pad}    <attr name=\"{}\" />\n",
                            xml_escape(&sui_intern::resolve(e.name))
                        ));
                    }
                    s.push_str(&format!("{pad}  </attrspat>\n"));
                    s
                }
            };
            format!("{pad}<function>\n{inner}{pad}</function>")
        }
        // A primop, or a partial application of one, is `<unevaluated />`.
        IrValue::Builtin(..) => format!("{pad}<unevaluated />"),
        // Unreachable: `force()` above resolves every thunk, and emitting
        // `<thunk />` here WAS the defect. Kept as a typed refusal rather than
        // a panic, so a future `force()` change fails loudly instead of
        // quietly re-inventing the element.
        IrValue::Thunk(_) => {
            return Err(IrEvalError::Unsupported(
                "toXML: force() returned a thunk",
            ));
        }
    })
}

/// `builtins.toXML` — prologue, the `<expr>` root CppNix always writes, and
/// the rendered tree. The root wrapper was missing, which made every call
/// wrong including the ones whose bodies looked right.
fn to_xml(v: &IrValue) -> Result<String, IrEvalError> {
    let mut ancestors: Vec<usize> = Vec::new();
    let body = value_to_xml(v, 2, &mut ancestors)?;
    Ok(format!(
        "<?xml version='1.0' encoding='utf-8'?>\n<expr>\n{body}\n</expr>\n"
    ))
}
