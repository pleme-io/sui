//! Typed border for nix's **laziness / thunk-sharing / demand-order**
//! model — the one core evaluator algorithm that, until now, lived
//! only as imperative Rust in `sui-eval/src/{value,eval,lazy}.rs` and
//! was hand-coded *twice* (the tree-walker + the bytecode VM).  That
//! double-authoring is exactly why the two engines diverge on byte
//! parity: the VM historically *defers* string-context while the
//! tree-walker *tracks* it, and thunk sharing is not referential, so
//! evaluation *order* can change a derivation (impossible in nix).
//!
//! This module names the evaluation model as a typed Lisp surface so
//! **both engines drive one authored spec** — per the ★★ TYPED-SPEC +
//! INTERPRETER TRIPLET directive, drift between two interpreters of
//! one algorithm becomes unrepresentable.  The *force hot-path* stays
//! in sui-eval (byte-parity + performance); this domain is the
//! authoring surface + the decision interpreter that the real engine
//! must satisfy.  `theory/BUILD.md` §II is the doctrine.
//!
//! ## Authoring surface
//!
//! ```lisp
//! (deflaziness-model
//!   :name           "cppnix"
//!   :force-strategy Whnf
//!   :sharing        Referential
//!   :string-context Tracked
//!   :demand-order   "nix")
//!
//! (defthunk-discipline
//!   :name            "recursive-binding"
//!   :recursive       true
//!   :reentry         Promise
//!   :memoize-on-force true)
//! ```

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

// ── Typed border — the evaluation model ────────────────────────────

/// How deeply a `force` reduces a thunk.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForceStrategy {
    /// Weak-head normal form — reduce to the outermost constructor
    /// only.  nix's `force`.
    Whnf,
    /// Fully evaluate (nix's `deepSeq` / the `--strict` path).
    Deep,
}

/// Thunk-sharing discipline — the invariant that makes evaluation
/// **order-independent**.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sharing {
    /// A thunk is computed once, memoized in a shared cell, and every
    /// reference observes that same cell.  nix's model — evaluation
    /// order can never change a derivation.
    Referential,
    /// A thunk is re-evaluated per reference (no shared memo).  The
    /// observed sui bug: warming `perl.override` changed libxcrypt's
    /// hash, because the same expression re-evaluated in a polluted
    /// context.  This variant names the *wrong* state so a spec that
    /// selects it is a visible red flag, not a silent default.
    PerSite,
}

/// String-context propagation discipline.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringContext {
    /// Context (the set of store paths a string depends on) rides with
    /// the string value through every operation.  Required for correct
    /// derivations — the tree-walker's behaviour.
    Tracked,
    /// Context resolution is deferred; paths interpolate raw.  The
    /// bytecode VM's historical behaviour → wrong derivations.  Named
    /// so a model that selects it is visibly non-parity.
    Deferred,
}

/// The canonical evaluation model both engines MUST drive.  One
/// authored value replaces two hand-coded force machineries.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "deflaziness-model")]
pub struct LazinessModel {
    pub name: String,
    #[serde(rename = "forceStrategy")]
    pub force_strategy: ForceStrategy,
    pub sharing: Sharing,
    #[serde(rename = "stringContext")]
    pub string_context: StringContext,
    /// The demand order the engine walks arguments/attrs in.  `"nix"`
    /// means: match cppnix's exact evaluation order byte-for-byte.
    #[serde(rename = "demandOrder")]
    pub demand_order: String,
}

impl LazinessModel {
    /// Whether this model is byte-parity-capable — the two invariants
    /// that, if violated, make derivations diverge from nix.  A model
    /// that is `PerSite` or `Deferred` cannot be byte-exact.
    #[must_use]
    pub fn is_parity_capable(&self) -> bool {
        self.sharing == Sharing::Referential
            && self.string_context == StringContext::Tracked
    }
}

// ── Typed border — per-thunk-kind discipline ───────────────────────

/// Re-entry policy when a `force` lands on a thunk already under
/// evaluation.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reentry {
    /// Non-recursive thunk re-entered ⇒ genuine infinite recursion.
    Blackhole,
    /// Self-recursive binding re-entered ⇒ a promise cell is returned
    /// (nix's `rec { … }` fixpoint tolerance).
    Promise,
}

/// How a specific thunk *kind* is forced + what re-entry means for it.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defthunk-discipline")]
pub struct ThunkDiscipline {
    /// `"non-recursive"` | `"recursive-binding"`.
    pub name: String,
    pub recursive: bool,
    pub reentry: Reentry,
    /// Whether forcing memoizes the value into the shared cell.  Under
    /// `Sharing::Referential` this MUST be true, or sharing is a lie.
    #[serde(rename = "memoizeOnForce")]
    pub memoize_on_force: bool,
}

// ── Interpreter — the force-discipline FSM (mockable Environment) ──

/// A thunk's identity in the store.  The real engine uses `Rc` cell
/// identity; the interpreter only needs a comparable id.
pub type ThunkId = u64;

/// An opaque stand-in for a computed WHNF value — the interpreter
/// enforces the *discipline*, not nix semantics, so the payload is
/// irrelevant to the decision logic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueRepr(pub String);

/// A thunk's evaluation state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThunkState {
    Unforced,
    InProgress,
    Forced(ValueRepr),
}

/// The result of a `force` under the model's discipline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForceOutcome {
    /// Freshly computed (and memoized, under referential sharing).
    Computed(ValueRepr),
    /// Returned the shared memoized value — the order-independence
    /// guarantee in action.
    MemoHit(ValueRepr),
    /// Recursive re-entry produced a promise cell.
    Recursed,
    /// Non-recursive re-entry — genuine infinite recursion.
    InfiniteRecursion,
}

/// The side-effecting surface the force interpreter needs.  Real
/// implementations back this with sui-eval's thunk store; tests mock
/// it.  The trait IS the testability contract (TYPED-SPEC triplet).
pub trait ThunkEnvironment {
    fn state(&self, id: ThunkId) -> ThunkState;
    /// Mark `id` as under evaluation (blackhole/promise territory).
    fn enter(&mut self, id: ThunkId);
    /// Memoise the computed value into the shared cell.
    fn memoize(&mut self, id: ThunkId, value: ValueRepr);
    /// Compute `id`'s WHNF value — the real reduction, mocked in tests.
    ///
    /// # Errors
    /// Propagates the underlying evaluation error.
    fn compute(&mut self, id: ThunkId) -> Result<ValueRepr, SpecError>;
}

/// Force a thunk under the model's + discipline's rules.  This is the
/// decision logic both engines must obey; the real WHNF reduction is
/// the `compute` callback.  The load-bearing rule: under referential
/// sharing an already-`Forced` thunk returns its memoised value, so a
/// second force in any order yields the same result — the exact
/// invariant sui's current engine violates.
///
/// # Errors
/// Propagates `compute` failures.
pub fn force<E: ThunkEnvironment>(
    model: &LazinessModel,
    discipline: &ThunkDiscipline,
    env: &mut E,
    id: ThunkId,
) -> Result<ForceOutcome, SpecError> {
    match env.state(id) {
        ThunkState::Forced(v) => Ok(ForceOutcome::MemoHit(v)),
        ThunkState::InProgress => Ok(match discipline.reentry {
            Reentry::Promise if discipline.recursive => ForceOutcome::Recursed,
            _ => ForceOutcome::InfiniteRecursion,
        }),
        ThunkState::Unforced => {
            env.enter(id);
            let v = env.compute(id)?;
            if discipline.memoize_on_force && model.sharing == Sharing::Referential {
                env.memoize(id, v.clone());
            }
            Ok(ForceOutcome::Computed(v))
        }
    }
}

// ── Canonical Lisp + loaders ───────────────────────────────────────

/// The embedded canonical laziness model + thunk disciplines.
pub const CANONICAL_LAZINESS_LISP: &str = include_str!("../specs/laziness.lisp");

/// Load every authored `(deflaziness-model …)`.
///
/// # Errors
/// Fails if the canonical Lisp doesn't parse under the schema.
pub fn load_canonical_models() -> Result<Vec<LazinessModel>, SpecError> {
    crate::loader::load_all::<LazinessModel>(CANONICAL_LAZINESS_LISP)
}

/// Load every authored `(defthunk-discipline …)`.
///
/// # Errors
/// Fails if the canonical Lisp doesn't parse under the schema.
pub fn load_canonical_disciplines() -> Result<Vec<ThunkDiscipline>, SpecError> {
    crate::loader::load_all::<ThunkDiscipline>(CANONICAL_LAZINESS_LISP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A mock thunk store: id → state, plus a scripted `compute`
    /// return + a call counter to prove memoisation prevents
    /// recomputation.
    struct MockEnv {
        states: HashMap<ThunkId, ThunkState>,
        compute_returns: String,
        compute_calls: u32,
    }

    impl ThunkEnvironment for MockEnv {
        fn state(&self, id: ThunkId) -> ThunkState {
            self.states.get(&id).cloned().unwrap_or(ThunkState::Unforced)
        }
        fn enter(&mut self, id: ThunkId) {
            self.states.insert(id, ThunkState::InProgress);
        }
        fn memoize(&mut self, id: ThunkId, value: ValueRepr) {
            self.states.insert(id, ThunkState::Forced(value));
        }
        fn compute(&mut self, _id: ThunkId) -> Result<ValueRepr, SpecError> {
            self.compute_calls += 1;
            Ok(ValueRepr(self.compute_returns.clone()))
        }
    }

    fn cppnix_model() -> LazinessModel {
        load_canonical_models()
            .unwrap()
            .into_iter()
            .find(|m| m.name == "cppnix")
            .expect("canonical cppnix model")
    }

    fn discipline(name: &str) -> ThunkDiscipline {
        load_canonical_disciplines()
            .unwrap()
            .into_iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("canonical discipline {name}"))
    }

    #[test]
    fn canonical_lisp_loads_model_and_disciplines() {
        assert!(!load_canonical_models().unwrap().is_empty());
        assert!(load_canonical_disciplines().unwrap().len() >= 2);
    }

    #[test]
    fn cppnix_model_is_parity_capable() {
        // The whole point: the authored model is Referential + Tracked.
        assert!(cppnix_model().is_parity_capable());
    }

    #[test]
    fn perinit_persite_model_is_not_parity_capable() {
        let bad = LazinessModel {
            name: "sui-bug".into(),
            force_strategy: ForceStrategy::Whnf,
            sharing: Sharing::PerSite,
            string_context: StringContext::Deferred,
            demand_order: "nix".into(),
        };
        assert!(!bad.is_parity_capable());
    }

    #[test]
    fn referential_sharing_memoises_then_reuses_no_recompute() {
        // The order-independence invariant: force once → Computed +
        // memoised; force again → MemoHit, WITHOUT recomputing.  This
        // is what sui's real engine must satisfy to be parity-correct.
        let model = cppnix_model();
        let disc = discipline("non-recursive");
        let mut env = MockEnv {
            states: HashMap::new(),
            compute_returns: "az4wk589".into(),
            compute_calls: 0,
        };
        let first = force(&model, &disc, &mut env, 1).unwrap();
        assert_eq!(first, ForceOutcome::Computed(ValueRepr("az4wk589".into())));
        let second = force(&model, &disc, &mut env, 1).unwrap();
        assert_eq!(second, ForceOutcome::MemoHit(ValueRepr("az4wk589".into())));
        assert_eq!(env.compute_calls, 1, "memoised thunk must not recompute");
    }

    #[test]
    fn recursive_binding_reentry_is_a_promise_not_infinite_recursion() {
        let model = cppnix_model();
        let disc = discipline("recursive-binding");
        let mut env = MockEnv {
            states: HashMap::from([(7, ThunkState::InProgress)]),
            compute_returns: String::new(),
            compute_calls: 0,
        };
        assert_eq!(force(&model, &disc, &mut env, 7).unwrap(), ForceOutcome::Recursed);
    }

    #[test]
    fn non_recursive_reentry_is_infinite_recursion() {
        let model = cppnix_model();
        let disc = discipline("non-recursive");
        let mut env = MockEnv {
            states: HashMap::from([(9, ThunkState::InProgress)]),
            compute_returns: String::new(),
            compute_calls: 0,
        };
        assert_eq!(
            force(&model, &disc, &mut env, 9).unwrap(),
            ForceOutcome::InfiniteRecursion
        );
    }
}
