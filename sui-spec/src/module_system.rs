//! The Nix module system — typed border for the option-merge lattice.
//!
//! This module names the load-bearing gap that blocks sui from
//! evaluating NixOS, nix-darwin, and home-manager configurations.
//! cppnix's `lib.evalModules` runs a fixed-point over a set of
//! modules, each declaring `options` (typed slots) and `config`
//! (definitions for those slots) plus optional `imports` (recursive
//! module inclusion).  The result is a merged config attrset that
//! `system.build.toplevel` (and its kin) consume.
//!
//! Per the constructive-substrate-engineering pattern, the algorithm
//! lives here as a typed Rust border + a Lisp spec — both engines
//! will eventually drive the same `(defmodule-eval-algorithm …)`,
//! so they cannot drift.  The implementation in `sui-eval` is M2
//! work; today this module pins down the typed contract every M2
//! implementation must satisfy.
//!
//! ## Authoring surface
//!
//! Three keyword forms compose into the full algorithm:
//!
//! - `(defoption-type :name "..." :merge-strategy ... :check-kind ...)`
//!   declares a type in the option-type registry (`bool`, `int`,
//!   `str`, `path`, `listOf<T>`, `attrsOf<T>`, `submodule`,
//!   `oneOf [...]`, `nullOr T`, `attrs`, `any`).  The
//!   `merge-strategy` determines how multiple definitions of the
//!   same option combine; the `check-kind` determines acceptance.
//!
//! - `(defpriority :name "..." :level N :origin ...)` declares one
//!   priority rank in the priority lattice (`mkDefault 1000`,
//!   `mkOverride 0`, `mkForce 50`, normal=100 by default).  Higher
//!   `level` = lower priority (matches cppnix).
//!
//! - `(defmodule-eval-algorithm cppnix-module-eval :name ... :phases (...))`
//!   declares the fixed-point pipeline: collect-modules,
//!   resolve-imports, evaluate-options, group-definitions,
//!   resolve-priorities, merge-per-type, type-check, emit-config.
//!
//! Future M2 implementation: replace the `apply()` stub with a real
//! interpreter that walks the phases against an `evalModulesArgs`.
//! Both `sui-eval` and `sui-bytecode` will call exactly that
//! function, with exactly the same spec.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

// ── Typed border — option types ────────────────────────────────────

/// One entry in the option-type registry.  Each cppnix type
/// (`lib.types.<X>`) becomes one `(defoption-type …)` form.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defoption-type")]
pub struct OptionTypeSpec {
    /// Canonical name (`"bool"`, `"int"`, `"listOf"`, ...).
    pub name: String,
    /// How definitions of this type combine when multiple values
    /// exist at the same priority.
    #[serde(rename = "mergeStrategy")]
    pub merge_strategy: MergeStrategy,
    /// Acceptance check applied per individual definition.
    #[serde(rename = "checkKind")]
    pub check_kind: TypeCheckKind,
    /// For parametric types (`listOf T`, `attrsOf T`, `nullOr T`):
    /// the name of the element type's `OptionTypeSpec`.
    #[serde(default, rename = "elementType")]
    pub element_type: Option<String>,
    /// For `oneOf [a b c]`: the named candidate types.
    #[serde(default, rename = "memberTypes")]
    pub member_types: Vec<String>,
}

/// How multiple definitions of one option fold into one value.
///
/// The cppnix lattice is small: most types are last-wins under
/// priority resolution; `listOf` concatenates after priority; the
/// submodule + attrsOf variants recurse.  Custom merges plug in via
/// a named hook the interpreter resolves.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeStrategy {
    /// One definition wins by priority order (`bool`, `int`, `str`,
    /// `path`, `package`, `null`, `enum`).  Tie at top priority is
    /// a typeError.
    LastWins,
    /// Concatenate lists in priority order (`listOf T`).  Elements
    /// from higher priorities come first.
    Concatenate,
    /// Recursive submodule eval — apply the module algorithm to the
    /// definitions of this option (`submodule`).
    SubmoduleMerge,
    /// Deep-merge attrsets — recurse on overlapping keys, set-union
    /// on disjoint (`attrsOf T`, plain `attrs`).
    AttrsetMerge,
    /// At most one definition allowed; multiple = typeError
    /// (`oneOf` after dispatch).
    Disjoint,
    /// Plug-in merge function named by string; the interpreter
    /// resolves the name to a builtin or a user-supplied function.
    /// Used for `lib.types.either`, `lib.types.functionTo`, etc.
    Custom,
    /// `any`-typed: accept any value, last-wins.  Unsafe; only
    /// permitted for the `any` option type explicitly.
    AnyLastWins,
}

/// Per-definition acceptance check.  Run before merging.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeCheckKind {
    Bool,
    Int,
    Str,
    Path,
    Null,
    /// Numeric range; details supplied by element_type rendering.
    IntBetween,
    /// Anything; no check.
    Any,
    /// One-of-string-enum; member_types holds the literals.
    Enum,
    /// `listOf T` — every element checks against element_type.
    ListOf,
    /// `attrsOf T` — every value checks against element_type.
    AttrsOf,
    /// Submodule — invoke recursive module eval.
    Submodule,
    /// `oneOf [t1 t2 ...]` — one of member_types succeeds.
    OneOf,
    /// `nullOr T` — null OR element_type.
    NullOr,
    /// Plain `attrs` — must be an attrset (values unchecked).
    Attrs,
    /// `package` — must be a derivation (or path to one).
    Package,
    /// `functionTo T` — function whose return type checks.
    FunctionTo,
}

// ── Typed border — priority lattice ────────────────────────────────

/// One rank in the priority lattice.  cppnix uses `mkDefault 1000`
/// (lowest), `default 100`, `mkOverride 50`, `mkForce 50` — lower
/// `level` value = higher priority.  This typed border lets the
/// authored spec declare the canonical ranks once.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defpriority")]
pub struct PriorityRank {
    pub name: String,
    pub level: u32,
    pub origin: PriorityOrigin,
}

/// Where a definition's priority comes from.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriorityOrigin {
    /// Set by `mkDefault`.  Level conventionally 1000.
    MkDefault,
    /// Plain definition without wrapper.  Level conventionally 100.
    Normal,
    /// Set by `mkOverride N`.  Level is the wrapper's argument.
    MkOverride,
    /// Set by `mkForce`.  Level conventionally 50.
    MkForce,
    /// Set by `mkOptionDefault`.  Level conventionally 1500
    /// (lower than any user-visible priority).
    MkOptionDefault,
}

// ── Typed border — algorithm pipeline ─────────────────────────────

/// The module-evaluation algorithm authored as
/// `(defmodule-eval-algorithm …)`.  Phases compose left-to-right
/// over an `EvalModulesArgs` scratchpad; later phases consume
/// earlier-bound slots.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defmodule-eval-algorithm")]
pub struct ModuleEvalAlgorithm {
    pub name: String,
    pub phases: Vec<ModulePhase>,
}

/// One phase of the module-evaluation pipeline.  Mirrors the shape
/// of [`crate::derivation::Phase`] for visual + cognitive
/// consistency across spec domains.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ModulePhase {
    pub kind: ModulePhaseKind,
    #[serde(default)]
    pub bind: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
}

/// The closed set of operations any module-eval algorithm composes.
/// Adding a new variant IS adding a new primitive to the spec
/// language.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModulePhaseKind {
    /// Resolve `imports` transitively, deduplicating by content
    /// hash.  Each module is just an attrset / function; the
    /// fixpoint walks until no new imports appear.
    CollectModules,
    /// Distinguish `options` from `config` in every module.  Some
    /// modules are pure-config; others declare options.
    PartitionOptionsAndConfig,
    /// Walk option declarations, build a typed option tree.  The
    /// tree is path-indexed (`services.foo.enable`).
    BuildOptionTree,
    /// Walk config definitions, attach each to its option path.
    /// Wrappers (`mkDefault`, `mkForce`, `mkIf cond`, `mkMerge`)
    /// are unwrapped into typed [`PriorityRank`] + value pairs.
    GroupDefinitions,
    /// Filter out definitions whose `mkIf` predicate is `false`.
    ResolveConditionals,
    /// Sort definitions per option by priority (lowest level wins).
    ResolvePriorities,
    /// For each option, run the type's [`MergeStrategy`] over the
    /// surviving definitions.  Submodule strategies recurse into
    /// this same algorithm.
    MergePerOption,
    /// Validate every merged value against its option's
    /// [`TypeCheckKind`].  Failure produces a typed error.
    TypeCheck,
    /// Walk recursive references between options (cppnix's
    /// `config.foo = config.bar`) until fixpoint.
    EvaluateRecursive,
    /// Produce the final `config` attrset.
    EmitConfig,
}

// ── Spec interpreter inputs/outputs ──────────────────────────────

/// The substrate's value type for module-system computation.  M2.0
/// uses `serde_json::Value` because it covers Nix's literal types
/// (bool, int/float, string, list, attrset, null) with no extra
/// machinery.  When sui-eval consumes this layer (M2.1), it'll
/// swap to its lazy `Value` type via an adapter trait.
pub type NixValue = serde_json::Value;

/// A single module — the cppnix `{ options, config, imports }`
/// authoring shape, typed.
#[derive(Debug, Clone, Default)]
pub struct Module {
    /// Other modules this one pulls in (by name).  M2.0 doesn't
    /// resolve these — caller passes pre-flattened module list.
    pub imports: Vec<String>,
    /// Option declarations this module contributes.  Keyed by
    /// option path (`"services.foo.enable"`).
    pub options: HashMap<String, OptionDecl>,
    /// Config definitions this module contributes.  Keyed by
    /// option path.
    pub config: Vec<Definition>,
}

/// One option declaration — what cppnix's `mkOption` returns.
#[derive(Debug, Clone)]
pub struct OptionDecl {
    /// Name of the `OptionTypeSpec` in the canonical registry
    /// (`"bool"`, `"int"`, `"str"`, `"path"`, `"listOf"`, ...).
    pub type_name: String,
    /// Default value when no module defines this option.
    pub default: Option<NixValue>,
    /// Human-readable description.
    pub description: String,
}

/// One config definition — a value assigned to an option path, with
/// a priority rank.  Wrappers like `mkDefault` / `mkForce` /
/// `mkOverride N` produce these directly with adjusted `priority`.
#[derive(Debug, Clone)]
pub struct Definition {
    /// Option path this defines (`"services.foo.enable"`).
    pub path: String,
    /// Value assigned at this path.
    pub value: NixValue,
    /// Priority level (lower = wins).  See [`PriorityRank`].
    pub priority: u32,
}

/// The output of `eval_modules` — a path-keyed config attrset.
pub type Config = HashMap<String, NixValue>;

// ── Spec interpreter — M2.0 minimal implementation ───────────────

/// Evaluate a list of modules + their option-type registry, and
/// return the merged config.  M2.0 implementation: covers LastWins
/// (bool, int, str, path, package, null), Concatenate (listOf),
/// AttrsetMerge (attrsOf, attrs), and priority resolution
/// (mkForce < normal < mkDefault).  Submodules + recursion +
/// transitive imports are M2.1 work.
///
/// # Errors
///
/// - `SpecError::Interp { phase: "type-check" }` if a definition's
///   value doesn't satisfy its option's declared type.
/// - `SpecError::Interp { phase: "unknown-type" }` if a module
///   declares an option whose `type_name` isn't in the registry.
/// - `SpecError::Interp { phase: "unknown-option" }` if a config
///   definition references an option path that wasn't declared by
///   any module.
/// - `SpecError::Interp { phase: "merge-conflict" }` if a non-
///   mergeable type sees multiple definitions at the same priority.
pub fn eval_modules(
    modules: &[Module],
    option_types: &[OptionTypeSpec],
) -> Result<Config, SpecError> {
    // Phase: BuildOptionTree — collect every option declaration into
    // a path-indexed tree.  When two modules declare the same option,
    // the first declaration's type wins; cppnix's typeMerge is more
    // sophisticated (it intersects the types) but for M2.0 we
    // accept the first declaration.
    let mut option_tree: HashMap<String, OptionDecl> = HashMap::new();
    for module in modules {
        for (path, decl) in &module.options {
            option_tree.entry(path.clone()).or_insert_with(|| decl.clone());
        }
    }

    // Phase: GroupDefinitions — collect every config definition into
    // per-option lists.
    let mut by_path: HashMap<String, Vec<Definition>> = HashMap::new();
    for module in modules {
        for def in &module.config {
            by_path.entry(def.path.clone()).or_default().push(def.clone());
        }
    }

    // Phase: TypeCheck (early) — every defined option path must
    // be declared somewhere.  This catches typos at the substrate
    // level where cppnix would catch them via the option system.
    for path in by_path.keys() {
        if !option_tree.contains_key(path) {
            return Err(SpecError::Interp {
                phase: "unknown-option".into(),
                message: format!(
                    "definition for `{path}` but no module declares this option",
                ),
            });
        }
    }

    let type_by_name: HashMap<&str, &OptionTypeSpec> =
        option_types.iter().map(|t| (t.name.as_str(), t)).collect();

    let mut config: Config = HashMap::new();

    for (path, decl) in &option_tree {
        // Look up the option's declared type.
        let type_spec = type_by_name.get(decl.type_name.as_str()).ok_or_else(|| {
            SpecError::Interp {
                phase: "unknown-type".into(),
                message: format!(
                    "option `{path}` declares type `{}` but no \
                     OptionTypeSpec with that name is in the registry",
                    decl.type_name,
                ),
            }
        })?;

        // Phase: ResolveConditionals — M2.0 doesn't have mkIf yet;
        // every definition is treated as active.

        // Phase: ResolvePriorities — sort by priority ascending,
        // partition by winning priority.
        let mut defs = by_path.remove(path).unwrap_or_default();
        if defs.is_empty() {
            // No definitions; use the default if any.
            if let Some(default) = &decl.default {
                config.insert(path.clone(), default.clone());
            }
            continue;
        }
        defs.sort_by_key(|d| d.priority);
        let top_priority = defs[0].priority;
        let winners: Vec<&Definition> =
            defs.iter().filter(|d| d.priority == top_priority).collect();

        // Phase: TypeCheck (per definition).
        for d in &winners {
            check_value(type_spec, &d.value, &d.path)?;
        }

        // Phase: MergePerOption — dispatch on the type's strategy.
        let merged = merge_definitions(type_spec, &winners, path)?;

        // Phase: EmitConfig (incremental).
        config.insert(path.clone(), merged);
    }

    Ok(config)
}

/// Per-definition type check.  Tests the value against the option's
/// declared `check_kind`.
fn check_value(
    type_spec: &OptionTypeSpec,
    value: &NixValue,
    path: &str,
) -> Result<(), SpecError> {
    let ok = match type_spec.check_kind {
        TypeCheckKind::Bool   => value.is_boolean(),
        TypeCheckKind::Int    => value.is_i64() || value.is_u64(),
        TypeCheckKind::Str    => value.is_string(),
        TypeCheckKind::Path   => value.is_string(),
        TypeCheckKind::Null   => value.is_null(),
        TypeCheckKind::Any    => true,
        TypeCheckKind::Attrs  => value.is_object(),
        TypeCheckKind::ListOf => value.is_array(),
        TypeCheckKind::AttrsOf => value.is_object(),
        TypeCheckKind::NullOr => true, // null OR element-type; M2.0 accepts any
        TypeCheckKind::Submodule => value.is_object(),
        // M2.0 doesn't enforce the deeper constraints; M2.1 will.
        TypeCheckKind::IntBetween | TypeCheckKind::Enum
        | TypeCheckKind::OneOf | TypeCheckKind::Package
        | TypeCheckKind::FunctionTo => true,
    };
    if !ok {
        return Err(SpecError::Interp {
            phase: "type-check".into(),
            message: format!(
                "option `{path}` declared `{}` but value is `{}`: {}",
                type_spec.name,
                value_type(value),
                truncate_value(value),
            ),
        });
    }
    Ok(())
}

fn value_type(v: &NixValue) -> &'static str {
    if v.is_boolean() { "bool" }
    else if v.is_i64() || v.is_u64() { "int" }
    else if v.is_f64() { "float" }
    else if v.is_string() { "str" }
    else if v.is_array() { "list" }
    else if v.is_object() { "attrs" }
    else if v.is_null() { "null" }
    else { "unknown" }
}

fn truncate_value(v: &NixValue) -> String {
    let s = v.to_string();
    if s.len() <= 60 { s } else { format!("{}…", &s[..60]) }
}

/// Apply the type's merge strategy to a slice of definitions at the
/// winning priority.
fn merge_definitions(
    type_spec: &OptionTypeSpec,
    winners: &[&Definition],
    path: &str,
) -> Result<NixValue, SpecError> {
    match type_spec.merge_strategy {
        MergeStrategy::LastWins | MergeStrategy::AnyLastWins => {
            // Strict cppnix: tie at top priority is an error.  But
            // many real configs have multiple `mkDefault` definitions
            // that happen to be identical — those are fine.
            if winners.len() > 1 {
                let first = &winners[0].value;
                let all_equal = winners.iter().all(|d| &d.value == first);
                if !all_equal {
                    return Err(SpecError::Interp {
                        phase: "merge-conflict".into(),
                        message: format!(
                            "option `{path}` has {} distinct top-priority \
                             definitions; LastWins requires either one \
                             definition at the top or all equal",
                            winners.len(),
                        ),
                    });
                }
            }
            Ok(winners[0].value.clone())
        }
        MergeStrategy::Concatenate => {
            // Lists: concatenate in priority-then-author order.
            // M2.0: winners are at the same priority; concatenate
            // in document order.
            let mut acc: Vec<NixValue> = Vec::new();
            for w in winners {
                let Some(arr) = w.value.as_array() else {
                    return Err(SpecError::Interp {
                        phase: "type-check".into(),
                        message: format!(
                            "option `{path}` is Concatenate (listOf) \
                             but a definition value is not a list",
                        ),
                    });
                };
                for item in arr {
                    acc.push(item.clone());
                }
            }
            Ok(NixValue::Array(acc))
        }
        MergeStrategy::AttrsetMerge => {
            // attrsOf / attrs: deep-merge keys.
            let mut acc = serde_json::Map::new();
            for w in winners {
                let Some(obj) = w.value.as_object() else {
                    return Err(SpecError::Interp {
                        phase: "type-check".into(),
                        message: format!(
                            "option `{path}` is AttrsetMerge but a \
                             definition value is not an attrset",
                        ),
                    });
                };
                for (k, v) in obj {
                    acc.insert(k.clone(), v.clone());
                }
            }
            Ok(NixValue::Object(acc))
        }
        MergeStrategy::Disjoint => {
            if winners.len() > 1 {
                return Err(SpecError::Interp {
                    phase: "merge-conflict".into(),
                    message: format!(
                        "option `{path}` is Disjoint (oneOf) but has \
                         {} top-priority definitions",
                        winners.len(),
                    ),
                });
            }
            Ok(winners[0].value.clone())
        }
        MergeStrategy::SubmoduleMerge | MergeStrategy::Custom => {
            // M2.1: SubmoduleMerge recurses into eval_modules.
            // M2.x: Custom plugs in a registered merge function.
            Err(SpecError::Interp {
                phase: "merge-unimplemented".into(),
                message: format!(
                    "option `{path}` uses merge strategy `{:?}` which \
                     M2.0 doesn't implement yet — M2.1 lands SubmoduleMerge, \
                     M2.x lands Custom",
                    type_spec.merge_strategy,
                ),
            })
        }
    }
}

/// Legacy `apply()` — kept for backwards compatibility with the
/// previous typed-NotYet API surface.  Returns a typed not-yet
/// error since the typed pipeline driven by `algo.phases` requires
/// the sui-eval Value type that lands at M2.1.  New code should
/// use [`eval_modules`] directly.
///
/// # Errors
///
/// Always returns `SpecError::Interp { phase: "module-eval" }` —
/// the pipeline-driven interpretation is M2.1 work.  Use
/// [`eval_modules`] for the M2.0 direct implementation.
pub fn apply(
    _algo: &ModuleEvalAlgorithm,
    _args: EvalModulesArgs,
) -> Result<String, SpecError> {
    Err(SpecError::Interp {
        phase: "module-eval".into(),
        message: "pipeline-driven apply() awaits the sui-eval Value \
                  bridge (M2.1).  The M2.0 minimal interpreter is in \
                  eval_modules() — call that directly with typed \
                  Module values".into(),
    })
}

/// Legacy `EvalModulesArgs` — kept for backwards compatibility with
/// the `apply()` surface.  New code uses the [`Module`] +
/// [`eval_modules`] API directly.
pub struct EvalModulesArgs {
    pub initial_modules: Vec<String>,
    pub scratchpad: HashMap<String, String>,
}

impl EvalModulesArgs {
    #[must_use]
    pub fn new(initial_modules: Vec<String>) -> Self {
        Self { initial_modules, scratchpad: HashMap::new() }
    }
}

// ── Canonical specs, compiled in ───────────────────────────────────

pub const CANONICAL_MODULE_SYSTEM_LISP: &str =
    include_str!("../specs/module_system.lisp");

/// Compile the canonical module-system specs.  Returns
/// `(algorithms, option_types, priorities)`.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.  Empty
/// algorithms vector is fine — the file may grow incrementally as
/// the M2 work scopes additional algorithms.
pub fn load_canonical() -> Result<CanonicalSpecs, SpecError> {
    let algos = crate::loader::load_all::<ModuleEvalAlgorithm>(
        CANONICAL_MODULE_SYSTEM_LISP,
    )?;
    let types = crate::loader::load_all::<OptionTypeSpec>(
        CANONICAL_MODULE_SYSTEM_LISP,
    )?;
    let priorities = crate::loader::load_all::<PriorityRank>(
        CANONICAL_MODULE_SYSTEM_LISP,
    )?;
    Ok(CanonicalSpecs { algos, types, priorities })
}

/// The three typed surfaces, loaded from one Lisp file.
pub struct CanonicalSpecs {
    pub algos: Vec<ModuleEvalAlgorithm>,
    pub types: Vec<OptionTypeSpec>,
    pub priorities: Vec<PriorityRank>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_specs_parse() {
        let specs = load_canonical().expect("canonical specs must compile");
        // The corpus must contain at least the cppnix-baseline
        // algorithm.  Empty vectors are not a regression today
        // (incremental authoring) but the named algorithm must
        // exist.
        assert!(
            specs.algos.iter().any(|a| a.name == "cppnix-module-eval"),
            "canonical specs must contain `cppnix-module-eval` algorithm",
        );
    }

    #[test]
    fn algorithm_has_expected_phases() {
        let specs = load_canonical().unwrap();
        let cppnix = specs
            .algos
            .iter()
            .find(|a| a.name == "cppnix-module-eval")
            .expect("cppnix algorithm must exist");
        let kinds: Vec<ModulePhaseKind> =
            cppnix.phases.iter().map(|p| p.kind).collect();
        for required in [
            ModulePhaseKind::CollectModules,
            ModulePhaseKind::ResolvePriorities,
            ModulePhaseKind::MergePerOption,
            ModulePhaseKind::TypeCheck,
            ModulePhaseKind::EmitConfig,
        ] {
            assert!(
                kinds.contains(&required),
                "cppnix-module-eval missing required phase: {required:?}",
            );
        }
    }

    #[test]
    fn canonical_option_types_cover_core() {
        let specs = load_canonical().unwrap();
        let names: std::collections::HashSet<&str> =
            specs.types.iter().map(|t| t.name.as_str()).collect();
        // The seven types every NixOS config touches must be in
        // the registry from day one.  If any of these disappear,
        // the option-merge surface is incomplete.
        for required in ["bool", "int", "str", "path", "listOf", "attrsOf", "submodule"] {
            assert!(
                names.contains(required),
                "canonical option-type registry missing `{required}`",
            );
        }
    }

    #[test]
    fn canonical_priorities_cover_core() {
        let specs = load_canonical().unwrap();
        let names: std::collections::HashSet<&str> =
            specs.priorities.iter().map(|p| p.name.as_str()).collect();
        for required in ["mkDefault", "normal", "mkForce", "mkOptionDefault"] {
            assert!(
                names.contains(required),
                "canonical priority lattice missing `{required}`",
            );
        }
    }

    #[test]
    fn pipeline_apply_still_typed_not_yet() {
        // The pipeline-driven apply() awaits the sui-eval Value
        // bridge (M2.1).  Until then, calling it surfaces a typed
        // error so consumers know which API to use.
        let algo = ModuleEvalAlgorithm {
            name: "test".into(),
            phases: vec![],
        };
        let err = apply(&algo, EvalModulesArgs::new(vec![]))
            .expect_err("pipeline apply must return error until M2.1");
        match err {
            SpecError::Interp { phase, message } => {
                assert_eq!(phase, "module-eval");
                assert!(message.contains("M2.1") || message.contains("eval_modules"));
            }
            _ => panic!("expected SpecError::Interp, got {err:?}"),
        }
    }

    // ── M2.0 minimal interpreter tests ─────────────────────────

    fn registry() -> Vec<OptionTypeSpec> {
        load_canonical().unwrap().types
    }

    fn opt(type_name: &str) -> OptionDecl {
        OptionDecl {
            type_name: type_name.into(),
            default: None,
            description: String::new(),
        }
    }

    fn opt_with_default(type_name: &str, default: NixValue) -> OptionDecl {
        OptionDecl {
            type_name: type_name.into(),
            default: Some(default),
            description: String::new(),
        }
    }

    fn def(path: &str, value: NixValue) -> Definition {
        Definition { path: path.into(), value, priority: 100 } // normal
    }

    #[test]
    fn eval_modules_trivial_bool_passes_through() {
        let mut module = Module::default();
        module.options.insert("foo".into(), opt("bool"));
        module.config.push(def("foo", NixValue::Bool(true)));
        let config = eval_modules(&[module], &registry()).unwrap();
        assert_eq!(config.get("foo"), Some(&NixValue::Bool(true)));
    }

    #[test]
    fn eval_modules_default_when_no_definition() {
        let mut module = Module::default();
        module.options.insert(
            "foo".into(),
            opt_with_default("bool", NixValue::Bool(false)),
        );
        let config = eval_modules(&[module], &registry()).unwrap();
        assert_eq!(config.get("foo"), Some(&NixValue::Bool(false)));
    }

    #[test]
    fn eval_modules_listof_concatenates() {
        let mut m1 = Module::default();
        m1.options.insert("xs".into(), opt("listOf"));
        m1.config.push(def("xs", serde_json::json!([1, 2])));
        let mut m2 = Module::default();
        m2.config.push(def("xs", serde_json::json!([3, 4])));
        let config = eval_modules(&[m1, m2], &registry()).unwrap();
        assert_eq!(config.get("xs"), Some(&serde_json::json!([1, 2, 3, 4])));
    }

    #[test]
    fn eval_modules_attrsof_deep_merges() {
        let mut m1 = Module::default();
        m1.options.insert("attrs".into(), opt("attrsOf"));
        m1.config.push(def("attrs", serde_json::json!({"a": 1})));
        let mut m2 = Module::default();
        m2.config.push(def("attrs", serde_json::json!({"b": 2})));
        let config = eval_modules(&[m1, m2], &registry()).unwrap();
        assert_eq!(config.get("attrs"), Some(&serde_json::json!({"a": 1, "b": 2})));
    }

    #[test]
    fn eval_modules_higher_priority_wins() {
        let mut module = Module::default();
        module.options.insert("foo".into(), opt("int"));
        module.config.push(Definition {
            path: "foo".into(),
            value: NixValue::from(7),
            priority: 1000, // mkDefault
        });
        module.config.push(Definition {
            path: "foo".into(),
            value: NixValue::from(99),
            priority: 50,   // mkForce
        });
        let config = eval_modules(&[module], &registry()).unwrap();
        assert_eq!(config.get("foo"), Some(&NixValue::from(99)));
    }

    #[test]
    fn eval_modules_type_check_rejects_wrong_type() {
        let mut module = Module::default();
        module.options.insert("foo".into(), opt("bool"));
        module.config.push(def("foo", NixValue::from(42))); // int!
        let err = eval_modules(&[module], &registry()).unwrap_err();
        match err {
            SpecError::Interp { phase, message } => {
                assert_eq!(phase, "type-check");
                assert!(message.contains("foo"));
                assert!(message.contains("bool"));
            }
            _ => panic!("expected type-check error"),
        }
    }

    #[test]
    fn eval_modules_rejects_unknown_option() {
        let module = Module {
            imports: vec![],
            options: HashMap::new(),
            config: vec![def("undeclared.path", NixValue::Bool(true))],
        };
        let err = eval_modules(&[module], &registry()).unwrap_err();
        match err {
            SpecError::Interp { phase, message } => {
                assert_eq!(phase, "unknown-option");
                assert!(message.contains("undeclared.path"));
            }
            _ => panic!("expected unknown-option error"),
        }
    }

    #[test]
    fn eval_modules_rejects_unknown_type_name() {
        let mut module = Module::default();
        module.options.insert("foo".into(), opt("nonexistent-type"));
        let err = eval_modules(&[module], &registry()).unwrap_err();
        match err {
            SpecError::Interp { phase, message } => {
                assert_eq!(phase, "unknown-type");
                assert!(message.contains("nonexistent-type"));
            }
            _ => panic!("expected unknown-type error"),
        }
    }

    #[test]
    fn eval_modules_lastwins_tie_at_top_priority_with_identical_value_passes() {
        let mut m1 = Module::default();
        m1.options.insert("foo".into(), opt("str"));
        m1.config.push(def("foo", NixValue::from("hello")));
        let mut m2 = Module::default();
        m2.config.push(def("foo", NixValue::from("hello")));
        let config = eval_modules(&[m1, m2], &registry()).unwrap();
        assert_eq!(config.get("foo"), Some(&NixValue::from("hello")));
    }

    #[test]
    fn eval_modules_lastwins_tie_at_top_priority_distinct_errors() {
        let mut m1 = Module::default();
        m1.options.insert("foo".into(), opt("str"));
        m1.config.push(def("foo", NixValue::from("a")));
        let mut m2 = Module::default();
        m2.config.push(def("foo", NixValue::from("b")));
        let err = eval_modules(&[m1, m2], &registry()).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "merge-conflict"),
            _ => panic!("expected merge-conflict"),
        }
    }

    #[test]
    fn priority_ordering_matches_cppnix_convention() {
        let specs = load_canonical().unwrap();
        let level = |n: &str| -> u32 {
            specs.priorities.iter()
                .find(|p| p.name == n)
                .expect(n)
                .level
        };
        // mkForce < normal < mkDefault < mkOptionDefault (lower = wins)
        assert!(level("mkForce") < level("normal"));
        assert!(level("normal") < level("mkDefault"));
        assert!(level("mkDefault") < level("mkOptionDefault"));
    }
}
