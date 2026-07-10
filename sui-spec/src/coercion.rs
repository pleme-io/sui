//! Typed border for nix's **value → string coercion** at the eval→drv
//! boundary — the complement of the `laziness` domain.  Where laziness
//! names *when/how* a thunk forces + shares, coercion names *how a
//! forced value becomes a string/path, and what store-context it
//! carries*.  Together they are why derivations byte-match nix.
//!
//! This domain captures three eval-parity roots sui already
//! byte-verified against the live nix oracle, turning tribal knowledge
//! into typed substrate (Operating Principle #7 — acquire + contextualise):
//!
//!   * **copy-to-store path coercion** — a path in an interpolation /
//!     derivation-attribute position is absolutised → canonicalised →
//!     existence-checked → NAR-copied to the store, yielding the store
//!     path *with* string-context.  The SAME path under `builtins.toString`
//!     stays **plain**: no copy, no context.  (sui root ec238d7.)
//!   * **fixed-output `out`** — a FOD's `out` env slot. (sui root 31f52e0.)
//!   * **string-context propagation** — context (the set of store paths a
//!     string depends on) merges through concat / interpolation, so a
//!     multi-output producer's `out` survives into the consumer's drv
//!     input set.  (sui root a67c244 — `concatStringsSep`.)
//!
//! Doctrine: `theory/BUILD.md` §II (the eval-model TYPED-SPEC triplet).
//!
//! ## Authoring surface
//!
//! ```lisp
//! (defcoercion-rule
//!   :name           "path-in-interpolation"
//!   :input-kind     "path"
//!   :context        Interpolation
//!   :copy-to-store  #t
//!   :tracks-context #t
//!   :yields         StorePathWithContext)
//! ```

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

// ── Typed border ───────────────────────────────────────────────────

/// The syntactic position a value is coerced in — this is what decides
/// copy-to-store + context tracking.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoercionContext {
    /// String interpolation `"${x}"` or a derivation attribute value.
    /// A path here copies-to-store and tracks context.
    Interpolation,
    /// `builtins.toString x` — a path here stays **plain**: no copy, no
    /// context.  The distinction that makes root ec238d7 byte-correct.
    ToString,
    /// A fixed-output derivation's `out` env slot (root 31f52e0).
    FodOutput,
}

/// What a value coerces to.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoercedForm {
    /// A `/nix/store/…` path carrying string-context.
    StorePathWithContext,
    /// A plain string with empty context.
    PlainString,
}

/// One coercion rule — `(kind, context)` ⇒ how it renders + whether it
/// copies to the store + whether context is tracked.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defcoercion-rule")]
pub struct CoercionRule {
    pub name: String,
    /// `"path"` | `"string"` | `"derivation"` | `"int"`.
    #[serde(rename = "inputKind")]
    pub input_kind: String,
    pub context: CoercionContext,
    #[serde(rename = "copyToStore")]
    pub copy_to_store: bool,
    #[serde(rename = "tracksContext")]
    pub tracks_context: bool,
    pub yields: CoercedForm,
}

impl CoercionRule {
    /// The load-bearing parity invariant: a path copies-to-store **iff**
    /// it also tracks context.  A rule that copies without tracking (or
    /// tracks without copying) would produce a drv whose input set
    /// disagrees with its interpolated text — a byte divergence.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        if self.input_kind == "path" {
            self.copy_to_store == self.tracks_context
        } else {
            true
        }
    }
}

// ── Interpreter — the coercion FSM behind a mockable store ─────────

/// The value being coerced + any context it already carries (for
/// propagation through concat / interpolation).
#[derive(Debug, Clone)]
pub struct CoerceInput {
    /// `"path"` | `"string"` | `"derivation"` | `"int"`.
    pub kind: String,
    /// The literal text (a filesystem path, a string body, …).
    pub literal: String,
    /// Context the value already carries (e.g. a derivation-output
    /// string's producing store path).
    pub prior_context: BTreeSet<String>,
}

/// The coerced result: the rendered text, its store-context, and
/// whether a store copy happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoercedValue {
    pub rendered: String,
    pub context: BTreeSet<String>,
    pub copied: bool,
}

/// The store side-effects coercion needs — mockable (TYPED-SPEC seam).
pub trait StoreEnvironment {
    /// Whether `path` exists (interpolation coercion requires it).
    fn exists(&self, path: &str) -> bool;
    /// NAR-copy `path`'s contents into the store, returning the
    /// `/nix/store/…` path.  Mocked in tests.
    ///
    /// # Errors
    /// Fails if the path can't be copied.
    fn copy_to_store(&mut self, path: &str) -> Result<String, SpecError>;
}

/// Coerce `input` under `rule`.  Encodes the three roots: interpolated
/// paths copy + track context; `toString` paths stay plain; prior
/// context always propagates.
///
/// # Errors
/// Fails if an interpolated path doesn't exist, or the store copy fails.
pub fn coerce<E: StoreEnvironment>(
    rule: &CoercionRule,
    env: &mut E,
    input: &CoerceInput,
) -> Result<CoercedValue, SpecError> {
    let mut context = input.prior_context.clone();

    if input.kind == "path" && rule.copy_to_store {
        if !env.exists(&input.literal) {
            return Err(SpecError::Interp {
                phase: "coerce-path".into(),
                message: format!("path {:?} does not exist", input.literal),
            });
        }
        let store_path = env.copy_to_store(&input.literal)?;
        if rule.tracks_context {
            context.insert(store_path.clone());
        }
        return Ok(CoercedValue {
            rendered: store_path,
            context,
            copied: true,
        });
    }

    // Non-copying coercion (toString paths, plain strings, ints):
    // render the literal, propagate any prior context unchanged.  A
    // `toString` path is exactly this branch — plain, no new context.
    Ok(CoercedValue {
        rendered: input.literal.clone(),
        context: if rule.tracks_context { context } else { BTreeSet::new() },
        copied: false,
    })
}

// ── Canonical Lisp + loader ────────────────────────────────────────

/// The embedded canonical coercion rules.
pub const CANONICAL_COERCION_LISP: &str = include_str!("../specs/coercion.lisp");

/// Load every authored `(defcoercion-rule …)`.
///
/// # Errors
/// Fails if the canonical Lisp doesn't parse under the schema.
pub fn load_canonical_rules() -> Result<Vec<CoercionRule>, SpecError> {
    crate::loader::load_all::<CoercionRule>(CANONICAL_COERCION_LISP)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mock store: everything exists; copy returns a deterministic
    /// `/nix/store/<hash>-<basename>` shape.
    struct MockStore;
    impl StoreEnvironment for MockStore {
        fn exists(&self, _path: &str) -> bool {
            true
        }
        fn copy_to_store(&mut self, path: &str) -> Result<String, SpecError> {
            let base = path.rsplit('/').next().unwrap_or(path);
            Ok(format!("/nix/store/deadbeef-{base}"))
        }
    }

    fn rule(name: &str) -> CoercionRule {
        load_canonical_rules()
            .unwrap()
            .into_iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("canonical rule {name}"))
    }

    #[test]
    fn canonical_rules_load_and_are_all_consistent() {
        let rules = load_canonical_rules().unwrap();
        assert!(rules.len() >= 3);
        for r in &rules {
            assert!(r.is_consistent(), "rule {} is inconsistent (copy≠track)", r.name);
        }
    }

    #[test]
    fn interpolated_path_copies_to_store_and_tracks_context() {
        // root ec238d7: a path in interpolation → store path + context.
        let mut env = MockStore;
        let out = coerce(
            &rule("path-in-interpolation"),
            &mut env,
            &CoerceInput {
                kind: "path".into(),
                literal: "/tmp/src/foo.c".into(),
                prior_context: BTreeSet::new(),
            },
        )
        .unwrap();
        assert!(out.copied);
        assert_eq!(out.rendered, "/nix/store/deadbeef-foo.c");
        assert!(out.context.contains("/nix/store/deadbeef-foo.c"));
    }

    #[test]
    fn tostring_path_stays_plain_no_copy_no_context() {
        // root ec238d7's other half: toString does NOT copy or track.
        let mut env = MockStore;
        let out = coerce(
            &rule("path-in-tostring"),
            &mut env,
            &CoerceInput {
                kind: "path".into(),
                literal: "/tmp/src/foo.c".into(),
                prior_context: BTreeSet::new(),
            },
        )
        .unwrap();
        assert!(!out.copied);
        assert_eq!(out.rendered, "/tmp/src/foo.c");
        assert!(out.context.is_empty());
    }

    #[test]
    fn string_context_propagates_through_coercion() {
        // root a67c244: a derivation-output string carries its producing
        // store path forward into the consumer's input set.
        let mut env = MockStore;
        let mut prior = BTreeSet::new();
        prior.insert("/nix/store/aaa-m.dev".to_string());
        let out = coerce(
            &rule("string-in-interpolation"),
            &mut env,
            &CoerceInput {
                kind: "string".into(),
                literal: "/nix/store/aaa-m.dev/include".into(),
                prior_context: prior,
            },
        )
        .unwrap();
        assert!(out.context.contains("/nix/store/aaa-m.dev"), "context must propagate");
    }

    #[test]
    fn interpolated_path_that_does_not_exist_errors() {
        struct Empty;
        impl StoreEnvironment for Empty {
            fn exists(&self, _p: &str) -> bool {
                false
            }
            fn copy_to_store(&mut self, _p: &str) -> Result<String, SpecError> {
                unreachable!()
            }
        }
        let mut env = Empty;
        let err = coerce(
            &rule("path-in-interpolation"),
            &mut env,
            &CoerceInput {
                kind: "path".into(),
                literal: "/nope".into(),
                prior_context: BTreeSet::new(),
            },
        );
        assert!(err.is_err());
    }
}
