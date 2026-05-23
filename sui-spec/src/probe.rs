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
//! Two canonical corpora compile into the binary:
//!
//! - [`CANONICAL_PROBES_LISP`] — `parity_probes.lisp`, the original
//!   seven cross-flake parity probes.
//! - [`CANONICAL_BUILTIN_SMOKE_LISP`] — `builtin_smoke_probes.lisp`,
//!   one probe per sui builtin module so a regression in any one of
//!   them shows up at sweep time without needing a real flake.
//!
//! Both produce values of the same [`Probe`] type — they differ only
//! in tag set + corpus location.  The [`BuiltinSmokeProbe`] wrapper
//! relabels the [`ProbeKind`] from `Eval` to `BuiltinSmoke` for
//! reporting; everything else is shared.
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

use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;
use crate::exec::CapturedOutput;
use crate::parity::{default_classify, ParityCheck, ProbeContext, ProbeKind, Verdict};

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

// ── ParityCheck impl ───────────────────────────────────────────────

impl ParityCheck for Probe {
    fn name(&self) -> &str { &self.name }
    fn tags(&self) -> &[String] { &self.tags }
    fn kind(&self) -> ProbeKind { ProbeKind::Eval }

    fn sui_invocation(&self, ctx: &ProbeContext, sui_bin: &Path) -> Command {
        let mut cmd = Command::new(sui_bin);
        let expr = ctx.substitute(&self.expr);
        cmd.args(["eval", "--json"]);
        cmd.arg(expr);
        cmd
    }

    fn nix_invocation(&self, ctx: &ProbeContext, nix_bin: &Path) -> Command {
        let mut cmd = Command::new(nix_bin);
        let expr = ctx.substitute(&self.expr);
        cmd.args([
            "eval", "--impure", "--json",
            "--extra-experimental-features", "nix-command flakes",
            "--expr",
        ]);
        cmd.arg(expr);
        cmd
    }

    fn classify(&self, sui: &CapturedOutput, nix: &CapturedOutput) -> Verdict {
        let mode = self.classify;
        default_classify(sui, nix, |s, n| {
            classify_outputs(mode, s.stdout.trim(), n.stdout.trim())
        })
    }
}

fn classify_outputs(mode: Classify, sui: &str, nix: &str) -> bool {
    match mode {
        Classify::JsonEqual => sui == nix,
        Classify::AttrNamesEqual => {
            let sui_v: Option<Vec<String>> = serde_json::from_str(sui).ok();
            let nix_v: Option<Vec<String>> = serde_json::from_str(nix).ok();
            match (sui_v, nix_v) {
                (Some(mut a), Some(mut b)) => {
                    a.sort();
                    b.sort();
                    a == b
                }
                _ => false,
            }
        }
        Classify::BothAreStorePaths => {
            let is_sp = |s: &str| s.trim_matches('"').starts_with("/nix/store/");
            is_sp(sui) && is_sp(nix)
        }
    }
}

// ── Wrapper for the builtin-smoke corpus ───────────────────────────

/// Newtype wrapper around [`Probe`] that re-labels [`ProbeKind`] for
/// the builtin-smoke corpus.  Lets the sweep report distinguish a
/// failing `(defprobe ...)` from `builtin_smoke_probes.lisp` from one
/// in `parity_probes.lisp` without authoring a parallel typed domain.
pub struct BuiltinSmokeProbe(pub Probe);

impl ParityCheck for BuiltinSmokeProbe {
    fn name(&self) -> &str { self.0.name() }
    fn tags(&self) -> &[String] { self.0.tags() }
    fn kind(&self) -> ProbeKind { ProbeKind::BuiltinSmoke }
    fn applies(&self, ctx: &ProbeContext) -> bool { self.0.applies(ctx) }
    fn sui_invocation(&self, ctx: &ProbeContext, sui_bin: &Path) -> Command {
        self.0.sui_invocation(ctx, sui_bin)
    }
    fn nix_invocation(&self, ctx: &ProbeContext, nix_bin: &Path) -> Command {
        self.0.nix_invocation(ctx, nix_bin)
    }
    fn classify(&self, sui: &CapturedOutput, nix: &CapturedOutput) -> Verdict {
        self.0.classify(sui, nix)
    }
}

// ── Canonical probe corpora ────────────────────────────────────────

pub const CANONICAL_PROBES_LISP: &str = include_str!("../specs/parity_probes.lisp");

pub const CANONICAL_BUILTIN_SMOKE_LISP: &str =
    include_str!("../specs/builtin_smoke_probes.lisp");

/// Compile the embedded probe corpus.  Returns every `(defprobe …)`
/// form in document order.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical() -> Result<Vec<Probe>, SpecError> {
    crate::loader::load_all::<Probe>(CANONICAL_PROBES_LISP)
}

/// Compile the builtin-smoke corpus — one probe per sui builtin module.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_builtin_smoke() -> Result<Vec<Probe>, SpecError> {
    crate::loader::load_all::<Probe>(CANONICAL_BUILTIN_SMOKE_LISP)
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

    #[test]
    fn builtin_smoke_corpus_parses() {
        let probes = load_builtin_smoke().expect("builtin-smoke corpus must compile");
        // The corpus targets sui-eval's 19 builtin modules; require at
        // least 19 probes so we know the sweep exercises each one.
        assert!(
            probes.len() >= 19,
            "builtin smoke corpus is sparse — got {}, expected ≥19 (one per module)",
            probes.len(),
        );
        for p in &probes {
            assert!(
                p.tags.iter().any(|t| t == "builtin-smoke"),
                "builtin-smoke probe {} missing :tags (\"builtin-smoke\" …)",
                p.name,
            );
        }
    }

    #[test]
    fn parity_check_impl_constructs_typed_invocation() {
        let probe = Probe {
            name: "test".into(),
            expr: "1 + 2".into(),
            classify: Classify::JsonEqual,
            tags: vec![],
        };
        let ctx = ProbeContext::current(std::path::PathBuf::from("/tmp/flake"));
        let cmd = probe.sui_invocation(&ctx, Path::new("/usr/local/bin/sui"));
        let argv = crate::exec::command_argv(&cmd);
        assert_eq!(argv[0], "/usr/local/bin/sui");
        assert_eq!(argv[1], "eval");
        assert!(argv.contains(&"1 + 2".to_string()));
    }
}
