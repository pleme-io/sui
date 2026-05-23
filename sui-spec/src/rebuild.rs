//! `(defrebuild-probe …)` — host-aware multi-stage rebuild parity probes.
//!
//! Where [`crate::probe::Probe`] tests a single Nix expression for parity,
//! a [`RebuildProbe`] tests *one stage of the rebuild pipeline* for parity
//! between sui and cppnix.  The set of stages was distilled from the
//! actual `fleet rebuild` driver in `pleme-io/fleet`:
//!
//! ```text
//! sui run .#rebuild
//!   ├── flake show          (FlakeShowKeys)
//!   ├── flake check         (FlakeCheckExit)
//!   ├── eval toplevel       (EvalToplevel { Darwin | NixOS })
//!   ├── eval home-manager   (EvalHomeActivation { user })
//!   ├── dry-run build       (DryRunClosure { Darwin | NixOS })
//!   ├── input lock hashes   (InputLockHash { input })
//!   ├── closure size        (ClosureSize { Darwin | NixOS })
//!   └── closure references  (ClosureReferenceGraph { Darwin | NixOS })
//! ```
//!
//! Each stage carries the kwargs it needs flatly on the probe — there is
//! no nested enum-with-data here, which keeps the Lisp authoring shape
//! consistent with [`crate::derivation::Phase`].  Stages that don't apply
//! to the operator's OS (e.g. `Darwin`-targeted probes on a Linux host)
//! self-skip via [`crate::parity::ParityCheck::applies`] and land as
//! `NotApplicable` records in the report.

use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;
use crate::exec::CapturedOutput;
use crate::parity::{
    default_classify, ParityCheck, ProbeContext, ProbeKind, TargetOs, Verdict,
};

// ── Typed border ───────────────────────────────────────────────────

/// One rebuild-stage parity probe.  Authored as `(defrebuild-probe ...)`.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defrebuild-probe")]
pub struct RebuildProbe {
    /// Stable name (must be unique within the rebuild corpus).
    pub name: String,
    /// Which hosts this probe applies to.
    #[serde(rename = "hostMode")]
    pub host_mode: HostMode,
    /// Required when `host_mode = Literal` — the hostname this probe
    /// is pinned to.
    #[serde(default, rename = "hostLiteral")]
    pub host_literal: Option<String>,
    /// Which stage of the rebuild pipeline this probe exercises.
    pub stage: RebuildStage,
    /// How sui's output is compared to nix's.
    pub compare: RebuildCompare,
    /// Tags for include/exclude filtering.  Conventional tags:
    /// `"smoke"`, `"rebuild-phase-1"`..`"rebuild-phase-4"`, `"expensive"`.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Host applicability selector.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostMode {
    /// Applies on whichever host the sweep is running on.
    Current,
    /// Applies only when `ctx.host == host_literal`.
    Literal,
    /// Applies on every host.  (Today the sweep only runs against the
    /// current host; this is the forward-looking case.)
    All,
}

/// Per-stage kwargs container.  Flat fields keep the Lisp authoring
/// surface simple — no nested attrset construction required.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RebuildStage {
    pub kind: RebuildStageKind,
    /// For `EvalToplevel` / `DryRunClosure` / `ClosureSize` /
    /// `ClosureReferenceGraph`: which top-level config to ask about.
    #[serde(default)]
    pub target: Option<ToplevelTarget>,
    /// For `EvalHomeActivation`: which user's `homeConfigurations.<user>`
    /// to evaluate.  Substituted from `ctx.user` if absent.
    #[serde(default)]
    pub user: Option<String>,
    /// For `InputLockHash`: which flake input to compare.
    #[serde(default)]
    pub input: Option<String>,
}

/// The set of rebuild stages the sweep knows how to drive.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RebuildStageKind {
    /// `flake show --json` on the flake.  Compare the top-level
    /// attribute name set.
    FlakeShowKeys,
    /// `flake check` on the flake.  Compare exit code.
    FlakeCheckExit,
    /// `eval --json '.<flake>#darwinConfigurations.<host>.system.build.toplevel.outPath'`
    /// (or the NixOS equivalent).  Compare as JSON.
    EvalToplevel,
    /// `eval --json '.<flake>#homeConfigurations.<user>.activationPackage.outPath'`.
    EvalHomeActivation,
    /// `build --dry-run --print-out-paths .<flake>#<toplevel>`.  Compare
    /// the printed store-path set.
    DryRunClosure,
    /// `eval --json '(getFlake "path:<flake>").inputs.<input>.narHash'`.
    InputLockHash,
    /// `eval --json 'builtins.length (builtins.attrNames ...)'` — a
    /// rough closure-size sentinel that exercises the module system
    /// without requiring a full build.  Compare as integer JSON.
    ClosureSize,
    /// Full closure reference set (after eval+build).  Marked
    /// `expensive` because it builds the toplevel derivation.
    ClosureReferenceGraph,
}

/// Which top-level system configuration to ask about.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToplevelTarget {
    /// `darwinConfigurations.<host>.system.build.toplevel`
    Darwin,
    /// `nixosConfigurations.<host>.config.system.build.toplevel`
    NixOS,
}

impl ToplevelTarget {
    /// Map to the [`TargetOs`] this target requires.
    #[must_use]
    pub fn required_os(self) -> TargetOs {
        match self {
            ToplevelTarget::Darwin => TargetOs::Darwin,
            ToplevelTarget::NixOS  => TargetOs::Linux,
        }
    }

    /// Render the `<flake>#<attr>` selector string (without the
    /// `outPath` / `.<sub>` suffix).
    #[must_use]
    pub fn flake_attr(self, host: &str) -> String {
        match self {
            ToplevelTarget::Darwin => {
                format!("darwinConfigurations.{host}.system.build.toplevel")
            }
            ToplevelTarget::NixOS => {
                format!("nixosConfigurations.{host}.config.system.build.toplevel")
            }
        }
    }
}

/// How the rebuild probe compares sui's output to nix's.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebuildCompare {
    /// Exit-code equality — succeed iff both engines exited with the
    /// same `success` status.
    ExitCode,
    /// JSON byte-equality.
    JsonEqual,
    /// Sort-order-insensitive comparison of a JSON array of strings.
    AttrNamesEqual,
    /// Both engines must produce the same set of `/nix/store/...`
    /// paths (sorted comparison).
    StorePathSet,
    /// Both engines must produce equal integer outputs.
    IntegerEqual,
    /// Both engines must produce an isomorphic reference graph.  M0:
    /// graph parity is approximated by store-path-set parity.
    GraphIsomorphic,
}

// ── ParityCheck impl ───────────────────────────────────────────────

impl ParityCheck for RebuildProbe {
    fn name(&self) -> &str { &self.name }
    fn tags(&self) -> &[String] { &self.tags }
    fn kind(&self) -> ProbeKind { ProbeKind::Rebuild }

    fn applies(&self, ctx: &ProbeContext) -> bool {
        // 1. host filter
        match self.host_mode {
            HostMode::Current | HostMode::All => {}
            HostMode::Literal => {
                let Some(literal) = self.host_literal.as_deref() else { return false; };
                if literal != ctx.host { return false; }
            }
        }
        // 2. stage's required OS, if any
        if let Some(target) = self.stage.target {
            if target.required_os() != ctx.os { return false; }
        }
        true
    }

    fn sui_invocation(&self, ctx: &ProbeContext, sui_bin: &Path) -> Command {
        let mut cmd = Command::new(sui_bin);
        match self.stage.kind {
            RebuildStageKind::FlakeShowKeys => {
                cmd.args(["flake", "show", "--json", &flake_ref(ctx)]);
            }
            RebuildStageKind::FlakeCheckExit => {
                cmd.args(["flake", "check", &flake_ref(ctx)]);
            }
            RebuildStageKind::EvalToplevel => {
                let attr = self.toplevel_attr(ctx);
                cmd.args(["eval", "--json", &installable(ctx, &format!("{attr}.outPath"))]);
            }
            RebuildStageKind::EvalHomeActivation => {
                let attr = self.home_activation_attr(ctx);
                cmd.args(["eval", "--json", &installable(ctx, &format!("{attr}.outPath"))]);
            }
            RebuildStageKind::DryRunClosure => {
                let attr = self.toplevel_attr(ctx);
                cmd.args([
                    "build", "--dry-run", "--print-out-paths", "--no-link",
                    &installable(ctx, &attr),
                ]);
            }
            RebuildStageKind::InputLockHash => {
                let input = self.stage.input.as_deref().unwrap_or("nixpkgs");
                let expr = format!(
                    "(builtins.getFlake \"path:{}\").inputs.{input}.narHash",
                    ctx.flake_path.display(),
                );
                cmd.args(["eval", "--impure", "--json", "--expr", &expr]);
            }
            RebuildStageKind::ClosureSize => {
                let attr = self.toplevel_attr(ctx);
                let expr = format!(
                    "builtins.length (builtins.attrNames ((builtins.getFlake \"path:{}\").{attr}))",
                    ctx.flake_path.display(),
                );
                cmd.args(["eval", "--impure", "--json", "--expr", &expr]);
            }
            RebuildStageKind::ClosureReferenceGraph => {
                // The reference-graph probe requires the toplevel to
                // build first; we approximate it for M0 by listing the
                // store paths that `--print-out-paths --dry-run`
                // surfaces.
                let attr = self.toplevel_attr(ctx);
                cmd.args([
                    "build", "--dry-run", "--print-out-paths", "--no-link",
                    &installable(ctx, &attr),
                ]);
            }
        }
        cmd
    }

    fn nix_invocation(&self, ctx: &ProbeContext, nix_bin: &Path) -> Command {
        let mut cmd = Command::new(nix_bin);
        // cppnix needs the experimental features incantation everywhere.
        let exp = ["--extra-experimental-features", "nix-command flakes"];
        match self.stage.kind {
            RebuildStageKind::FlakeShowKeys => {
                cmd.args(["flake", "show", "--json"]);
                cmd.args(exp);
                cmd.arg(flake_ref(ctx));
            }
            RebuildStageKind::FlakeCheckExit => {
                cmd.args(["flake", "check"]);
                cmd.args(exp);
                cmd.arg(flake_ref(ctx));
            }
            RebuildStageKind::EvalToplevel => {
                let attr = self.toplevel_attr(ctx);
                cmd.args(["eval", "--impure", "--json"]);
                cmd.args(exp);
                cmd.arg(installable(ctx, &format!("{attr}.outPath")));
            }
            RebuildStageKind::EvalHomeActivation => {
                let attr = self.home_activation_attr(ctx);
                cmd.args(["eval", "--impure", "--json"]);
                cmd.args(exp);
                cmd.arg(installable(ctx, &format!("{attr}.outPath")));
            }
            RebuildStageKind::DryRunClosure => {
                let attr = self.toplevel_attr(ctx);
                cmd.args(["build", "--dry-run", "--print-out-paths", "--no-link"]);
                cmd.args(exp);
                cmd.arg(installable(ctx, &attr));
            }
            RebuildStageKind::InputLockHash => {
                let input = self.stage.input.as_deref().unwrap_or("nixpkgs");
                let expr = format!(
                    "(builtins.getFlake \"path:{}\").inputs.{input}.narHash",
                    ctx.flake_path.display(),
                );
                cmd.args(["eval", "--impure", "--json"]);
                cmd.args(exp);
                cmd.args(["--expr", &expr]);
            }
            RebuildStageKind::ClosureSize => {
                let attr = self.toplevel_attr(ctx);
                let expr = format!(
                    "builtins.length (builtins.attrNames ((builtins.getFlake \"path:{}\").{attr}))",
                    ctx.flake_path.display(),
                );
                cmd.args(["eval", "--impure", "--json"]);
                cmd.args(exp);
                cmd.args(["--expr", &expr]);
            }
            RebuildStageKind::ClosureReferenceGraph => {
                let attr = self.toplevel_attr(ctx);
                cmd.args(["build", "--dry-run", "--print-out-paths", "--no-link"]);
                cmd.args(exp);
                cmd.arg(installable(ctx, &attr));
            }
        }
        cmd
    }

    fn classify(&self, sui: &CapturedOutput, nix: &CapturedOutput) -> Verdict {
        let compare = self.compare;
        default_classify(sui, nix, |s, n| {
            compare_outputs(compare, s.stdout.trim(), n.stdout.trim(), s, n)
        })
    }
}

// ── Selector helpers ───────────────────────────────────────────────

impl RebuildProbe {
    fn toplevel_attr(&self, ctx: &ProbeContext) -> String {
        let target = self.stage.target.unwrap_or_else(|| match ctx.os {
            TargetOs::Darwin => ToplevelTarget::Darwin,
            _                => ToplevelTarget::NixOS,
        });
        target.flake_attr(&ctx.host)
    }

    fn home_activation_attr(&self, ctx: &ProbeContext) -> String {
        let user = self.stage.user.as_deref().unwrap_or(ctx.user.as_str());
        format!("homeConfigurations.\"{user}\".activationPackage")
    }
}

fn flake_ref(ctx: &ProbeContext) -> String {
    format!("path:{}", ctx.flake_path.display())
}

fn installable(ctx: &ProbeContext, attr: &str) -> String {
    format!("path:{}#{attr}", ctx.flake_path.display())
}

// ── Comparison dispatch ────────────────────────────────────────────

fn compare_outputs(
    mode: RebuildCompare,
    sui: &str,
    nix: &str,
    sui_out: &CapturedOutput,
    nix_out: &CapturedOutput,
) -> bool {
    match mode {
        RebuildCompare::ExitCode => sui_out.success == nix_out.success,
        RebuildCompare::JsonEqual => sui == nix,
        RebuildCompare::AttrNamesEqual => {
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
        RebuildCompare::StorePathSet | RebuildCompare::GraphIsomorphic => {
            // Both engines print one path per line; we set-compare.
            let mut s: Vec<&str> = sui.lines()
                .filter(|l| l.starts_with("/nix/store/"))
                .collect();
            let mut n: Vec<&str> = nix.lines()
                .filter(|l| l.starts_with("/nix/store/"))
                .collect();
            s.sort();
            n.sort();
            s == n
        }
        RebuildCompare::IntegerEqual => {
            let sn: Option<i64> = serde_json::from_str(sui).ok();
            let nn: Option<i64> = serde_json::from_str(nix).ok();
            matches!((sn, nn), (Some(a), Some(b)) if a == b)
        }
    }
}

// ── Canonical corpus, compiled in ──────────────────────────────────

pub const CANONICAL_REBUILD_PROBES_LISP: &str =
    include_str!("../specs/rebuild_probes.lisp");

/// Compile the canonical rebuild-probe corpus.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical() -> Result<Vec<RebuildProbe>, SpecError> {
    crate::loader::load_all::<RebuildProbe>(CANONICAL_REBUILD_PROBES_LISP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn canonical_rebuild_corpus_parses() {
        let probes = load_canonical().expect("canonical rebuild probes must compile");
        assert!(!probes.is_empty(), "rebuild corpus must contain at least one probe");
        for p in &probes {
            assert!(!p.name.is_empty(), "probe must have a name: {p:?}");
        }
    }

    #[test]
    fn canonical_rebuild_corpus_has_expected_stages() {
        let probes = load_canonical().unwrap();
        let stages: std::collections::HashSet<RebuildStageKind> =
            probes.iter().map(|p| p.stage.kind).collect();
        // The five baselines the operator picked + the three extra
        // categories.  If any of these is missing, the M0 corpus
        // regressed.
        for required in [
            RebuildStageKind::FlakeShowKeys,
            RebuildStageKind::FlakeCheckExit,
            RebuildStageKind::EvalToplevel,
            RebuildStageKind::DryRunClosure,
            RebuildStageKind::InputLockHash,
        ] {
            assert!(stages.contains(&required), "missing stage {required:?}");
        }
    }

    #[test]
    fn host_literal_filters() {
        let probe = RebuildProbe {
            name: "rio-only".into(),
            host_mode: HostMode::Literal,
            host_literal: Some("rio".into()),
            stage: RebuildStage {
                kind: RebuildStageKind::FlakeShowKeys,
                target: None,
                user: None,
                input: None,
            },
            compare: RebuildCompare::AttrNamesEqual,
            tags: vec![],
        };
        let ctx_rio = mk_ctx("rio", TargetOs::Linux);
        let ctx_cid = mk_ctx("cid", TargetOs::Darwin);
        assert!(probe.applies(&ctx_rio));
        assert!(!probe.applies(&ctx_cid));
    }

    #[test]
    fn target_os_filters_skip_mismatch() {
        let probe = RebuildProbe {
            name: "darwin-only".into(),
            host_mode: HostMode::Current,
            host_literal: None,
            stage: RebuildStage {
                kind: RebuildStageKind::EvalToplevel,
                target: Some(ToplevelTarget::Darwin),
                user: None,
                input: None,
            },
            compare: RebuildCompare::JsonEqual,
            tags: vec![],
        };
        assert!(probe.applies(&mk_ctx("cid", TargetOs::Darwin)));
        assert!(!probe.applies(&mk_ctx("rio", TargetOs::Linux)));
    }

    #[test]
    fn toplevel_attr_renders_correctly() {
        let darwin = ToplevelTarget::Darwin.flake_attr("cid");
        let linux = ToplevelTarget::NixOS.flake_attr("rio");
        assert_eq!(darwin, "darwinConfigurations.cid.system.build.toplevel");
        assert_eq!(linux, "nixosConfigurations.rio.config.system.build.toplevel");
    }

    #[test]
    fn sui_invocation_includes_flake_path() {
        let probe = mk_probe(RebuildStageKind::FlakeShowKeys, None);
        let ctx = mk_ctx("cid", TargetOs::Darwin);
        let cmd = probe.sui_invocation(&ctx, Path::new("/usr/local/bin/sui"));
        let argv = crate::exec::command_argv(&cmd);
        assert_eq!(argv[1], "flake");
        assert_eq!(argv[2], "show");
        assert!(argv.iter().any(|a| a.contains("/tmp/flake")));
    }

    #[test]
    fn nix_invocation_includes_experimental_features() {
        let probe = mk_probe(RebuildStageKind::FlakeShowKeys, None);
        let ctx = mk_ctx("cid", TargetOs::Darwin);
        let cmd = probe.nix_invocation(&ctx, Path::new("/usr/bin/nix"));
        let argv = crate::exec::command_argv(&cmd);
        assert!(argv.iter().any(|a| a == "--extra-experimental-features"));
    }

    fn mk_ctx(host: &str, os: TargetOs) -> ProbeContext {
        ProbeContext {
            flake_path: PathBuf::from("/tmp/flake"),
            flake_label: "flake".into(),
            host: host.into(),
            system: match os {
                TargetOs::Darwin => "aarch64-darwin".into(),
                TargetOs::Linux  => "x86_64-linux".into(),
                TargetOs::Other  => "unknown".into(),
            },
            user: "drzzln".into(),
            os,
        }
    }

    fn mk_probe(kind: RebuildStageKind, target: Option<ToplevelTarget>) -> RebuildProbe {
        RebuildProbe {
            name: "test".into(),
            host_mode: HostMode::Current,
            host_literal: None,
            stage: RebuildStage { kind, target, user: None, input: None },
            compare: RebuildCompare::JsonEqual,
            tags: vec!["test".into()],
        }
    }

    #[test]
    fn integer_equal_compare() {
        let sui = CapturedOutput {
            exit_code: Some(0), success: true,
            stdout: "42".into(), stderr: String::new(),
            duration: std::time::Duration::ZERO, timed_out: false,
        };
        let nix = sui.clone();
        let mut nix_diff = nix.clone();
        nix_diff.stdout = "43".into();
        assert!(compare_outputs(RebuildCompare::IntegerEqual, "42", "42", &sui, &nix));
        assert!(!compare_outputs(RebuildCompare::IntegerEqual, "42", "43", &sui, &nix_diff));
    }

    #[test]
    fn store_path_set_compare_sorts() {
        let sui_out = "/nix/store/aaaa-x\n/nix/store/bbbb-y\n";
        let nix_out = "/nix/store/bbbb-y\n/nix/store/aaaa-x\n";
        let dummy = CapturedOutput {
            exit_code: Some(0), success: true,
            stdout: String::new(), stderr: String::new(),
            duration: std::time::Duration::ZERO, timed_out: false,
        };
        assert!(compare_outputs(RebuildCompare::StorePathSet, sui_out, nix_out, &dummy, &dummy));
    }
}
