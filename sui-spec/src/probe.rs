//! Parity probes — each a typed question of the form
//! "does sui agree with CppNix on this expression?".
//!
//! The sweep harness used to be a bash script enumerating probes
//! inline.  It's now a Lisp corpus interpreted by a small Rust
//! runner.  Authoring a new probe is one `(defprobe …)` form;
//! promoting a probe to a regression guard is one `:tags ("regression")`
//! edit.  No bash, no JSON munging at the shell level — the runner
//! reads typed probes, executes both engines, and classifies.
//!
//! ## Authoring surface
//!
//! ```lisp
//! (defprobe
//!   :name     "getflake-outPath"
//!   :expr     "(builtins.getFlake \"path:$FLAKE\").outPath"
//!   :classify JsonEqual
//!   :tags     ("smoke" "drop-in-replacement"))
//! ```
//!
//! The `$FLAKE` token in `expr` is substituted at run time with
//! each flake's absolute path.  `classify` determines how the two
//! engines' outputs are compared.

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

/// A single parity probe — an expression to evaluate and a rule
/// for comparing outputs.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defprobe")]
pub struct Probe {
    pub name: String,
    pub expr: String,
    pub classify: Classify,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// How a probe compares sui's result to CppNix's.  Enum variants
/// are the typed border — the runner interprets exactly these
/// cases, and adding a new one is adding a new primitive to the
/// spec language.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classify {
    /// Byte-exact JSON equality after parsing each engine's stdout.
    JsonEqual,
    /// Attribute names only, order-insensitive (for attrset probes
    /// where the value at each key may differ but the keyset should
    /// match).
    AttrNamesEqual,
    /// Both engines must produce a `/nix/store/...` path with the
    /// store-path format.  Does not require the paths to match
    /// (useful while the outPath store-copy bug is open).
    BothAreStorePaths,
}

// ── Canonical probe corpus ──────────────────────────────────────────

pub const CANONICAL_PROBES_LISP: &str = include_str!("../specs/parity_probes.lisp");

/// Compile the embedded probe corpus.  Returns every `(defprobe …)`
/// form in document order.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical() -> Result<Vec<Probe>, SpecError> {
    crate::loader::load_all::<Probe>(CANONICAL_PROBES_LISP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_corpus_parses() {
        let probes = load_canonical().expect("canonical probes must compile");
        assert!(!probes.is_empty(), "corpus must contain at least one probe");
        // Every probe has a non-empty name and a `$FLAKE` placeholder
        // in the expression — that placeholder is how the runner
        // knows where to substitute each flake path.
        for p in &probes {
            assert!(!p.name.is_empty(), "probe must have a name: {p:?}");
            assert!(p.expr.contains("$FLAKE"),
                "probe {} must contain $FLAKE placeholder", p.name);
        }
    }

    #[test]
    fn canonical_corpus_has_the_flake_shape_probes() {
        let probes = load_canonical().unwrap();
        let names: Vec<&str> = probes.iter().map(|p| p.name.as_str()).collect();
        // These are the probes that caught the original bugs the
        // refactor fixed.  If they disappear the regression guard
        // is gone too.
        for required in ["getflake-outPath", "getflake-outputs-keys"] {
            assert!(names.contains(&required),
                "canonical corpus must contain {required}: have {names:?}");
        }
    }
}
