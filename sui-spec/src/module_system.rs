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

// ── Spec interpreter (M2 stub) ────────────────────────────────────

/// Inputs to a module-eval run.  Filled by the caller; phases
/// progressively populate the typed slots below.
pub struct EvalModulesArgs {
    /// Modules to evaluate.  In real impl this is an opaque value
    /// the engine carries; here it's a placeholder.
    pub initial_modules: Vec<String>,
    /// Per-option named scratchpads (intentionally typed as
    /// `String` placeholders; the M2 impl will swap to typed
    /// `Value`).
    pub scratchpad: HashMap<String, String>,
}

impl EvalModulesArgs {
    #[must_use]
    pub fn new(initial_modules: Vec<String>) -> Self {
        Self { initial_modules, scratchpad: HashMap::new() }
    }
}

/// Apply the module-eval algorithm.  M2 stub — returns
/// [`SpecError::Interp`] with `phase = "module-eval-unimplemented"`,
/// so any code that calls into this surface gets a typed,
/// surface-able "not yet" instead of silently passing.
///
/// # Errors
///
/// Always returns `SpecError::Interp` until M2 implementation lands.
/// The error message names the load-bearing gap so consumers know
/// exactly what's missing.
pub fn apply(
    _algo: &ModuleEvalAlgorithm,
    _args: EvalModulesArgs,
) -> Result<String, SpecError> {
    Err(SpecError::Interp {
        phase: "module-eval".into(),
        message: "module-system implementation not yet landed — \
                  the spec is authored, the typed border is in place, \
                  M2 work will provide the interpreter".into(),
    })
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
    fn apply_is_a_typed_not_yet() {
        // Until M2, apply() returns a typed error rather than silently
        // returning a placeholder value.  This makes the gap surfaceable
        // in any code path that calls into module-eval.
        let algo = ModuleEvalAlgorithm {
            name: "test".into(),
            phases: vec![],
        };
        let err = apply(&algo, EvalModulesArgs::new(vec![]))
            .expect_err("apply must return error until M2 lands");
        match err {
            SpecError::Interp { phase, message } => {
                assert_eq!(phase, "module-eval");
                assert!(message.contains("not yet landed"));
            }
            _ => panic!("expected SpecError::Interp, got {err:?}"),
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
