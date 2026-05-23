//! Activation script generation — typed border for the cppnix
//! `switch-to-configuration` / `darwin-rebuild` / `home-manager
//! activate` script generators.
//!
//! Given an evaluated NixOS / nix-darwin / home-manager config, the
//! activation phase produces a typed bundle:
//!
//! - the activation script text (and its store path)
//! - the per-host generation metadata (number, hash, profile path)
//! - the realised closure of files in `/etc`, systemd units, etc.
//!
//! Today the cppnix Nix expressions in `nixos-rebuild` /
//! `nix-darwin` / `home-manager` produce this through hand-coded
//! `pkgs.runCommand` derivations.  Per the spec pattern this module
//! names the typed contract: three algorithms, each authored as a
//! phase pipeline, drive the activation surface uniformly.
//!
//! M2 status: typed border + Lisp spec; interpreter is the M3 step
//! (depends on a working module system).
//!
//! ## Authoring surface
//!
//! ```lisp
//! (defactivation-script-algorithm cppnix-darwin
//!   :name   "cppnix-darwin"
//!   :target Darwin
//!   :phases ((:kind ResolveSystemBuildToplevel)
//!            (:kind GenerateLaunchdPlists)
//!            (:kind GenerateEtcSymlinks)
//!            (:kind ResolveSecretRefs)
//!            (:kind ComposeActivationScript :bind "script")
//!            (:kind WriteActivationDerivation :from "script")))
//! ```

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

// ── Typed border ───────────────────────────────────────────────────

/// One activation-script algorithm authored as
/// `(defactivation-script-algorithm …)`.  Variants by `target` map
/// to the three cppnix surfaces (NixOS, nix-darwin, home-manager).
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defactivation-script-algorithm")]
pub struct ActivationScriptAlgorithm {
    pub name: String,
    pub target: ActivationTarget,
    pub phases: Vec<ActivationPhase>,
}

/// Which system surface the algorithm targets.  The phase set is
/// largely shared, but the launchd / systemd / per-user distinction
/// determines a few platform-specific phases.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActivationTarget {
    /// NixOS (`/run/current-system`, systemd, `switch-to-configuration`).
    NixOS,
    /// nix-darwin (`/run/current-system`, launchd daemons, `darwin-rebuild`).
    Darwin,
    /// home-manager (per-user activation, launchd user agents or
    /// systemd user units, `home-manager activate`).
    HomeManager,
}

/// One phase of an activation pipeline.  Same flat-kwarg shape as
/// [`crate::derivation::Phase`] for visual + cognitive consistency.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ActivationPhase {
    pub kind: ActivationPhaseKind,
    #[serde(default)]
    pub bind: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
}

/// Closed set of activation phases.  Adding a variant IS extending
/// the spec language.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationPhaseKind {
    /// Evaluate `<config>.system.build.toplevel` for the host —
    /// the entry point for everything else.  Requires the module
    /// system (see [`crate::module_system`]).
    ResolveSystemBuildToplevel,
    /// Generate the systemd unit set from `services.*` modules.
    /// NixOS + home-manager (linux user) only.
    GenerateSystemdUnits,
    /// Generate launchd plists (`/Library/LaunchDaemons/*.plist`,
    /// `~/Library/LaunchAgents/*.plist`).  Darwin + home-manager
    /// (mac user) only.
    GenerateLaunchdPlists,
    /// Generate the `/etc` symlink farm for the new generation.
    /// NixOS + Darwin only.
    GenerateEtcSymlinks,
    /// Resolve any `SecretRef` value into its materialised cipher
    /// path before they cross into the activation script.
    ResolveSecretRefs,
    /// Compose the activation script text — concatenate per-module
    /// activation snippets in topological order.
    ComposeActivationScript,
    /// Write the activation script + closure as a sui-built
    /// derivation, returning the store path.
    WriteActivationDerivation,
    /// Hash the final activation closure for the OutcomeChain.
    /// Optional — not every host attaches; per-cluster policy.
    AttestClosureToChain,
}

// ── Spec interpreter (M3 stub) ─────────────────────────────────────

/// Inputs to an activation-script algorithm.  The scratchpad
/// progressively fills as phases run.
pub struct ActivationArgs {
    /// Path to the evaluated config (typically the `toplevel.outPath`
    /// that came out of module-eval).  M3 will replace this with
    /// the typed `Value` shape.
    pub toplevel_path: String,
    /// Hostname the activation will install on (`hostname -s`).
    pub host: String,
    /// Username for HomeManager target; ignored otherwise.
    pub user: String,
}

/// Apply the activation algorithm.  M3 stub — returns
/// [`SpecError::Interp`] with `phase = "activation-unimplemented"`.
///
/// # Errors
///
/// Always until the M3 implementation.  Stable error shape so
/// downstream code can detect the missing step.
pub fn apply(
    _algo: &ActivationScriptAlgorithm,
    _args: ActivationArgs,
) -> Result<String, SpecError> {
    Err(SpecError::Interp {
        phase: "activation".into(),
        message: "activation-script implementation not yet landed — \
                  the typed border + Lisp spec is in place, M3 work \
                  depends on the module-system M2 milestone".into(),
    })
}

// ── Canonical spec, compiled in ────────────────────────────────────

pub const CANONICAL_ACTIVATION_LISP: &str =
    include_str!("../specs/activation_script.lisp");

/// Compile the canonical activation specs.  Returns one
/// [`ActivationScriptAlgorithm`] per target.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical() -> Result<Vec<ActivationScriptAlgorithm>, SpecError> {
    crate::loader::load_all::<ActivationScriptAlgorithm>(
        CANONICAL_ACTIVATION_LISP,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_specs_parse() {
        let algos = load_canonical().expect("canonical activation specs must compile");
        assert!(
            !algos.is_empty(),
            "canonical activation corpus must contain at least one algorithm",
        );
    }

    #[test]
    fn three_targets_covered() {
        let algos = load_canonical().unwrap();
        let targets: std::collections::HashSet<ActivationTarget> =
            algos.iter().map(|a| a.target).collect();
        for required in [
            ActivationTarget::NixOS,
            ActivationTarget::Darwin,
            ActivationTarget::HomeManager,
        ] {
            assert!(
                targets.contains(&required),
                "missing activation algorithm for target {required:?}",
            );
        }
    }

    #[test]
    fn every_algorithm_resolves_then_composes_then_writes() {
        let algos = load_canonical().unwrap();
        for algo in &algos {
            let kinds: Vec<ActivationPhaseKind> =
                algo.phases.iter().map(|p| p.kind).collect();
            // The triplet that EVERY activation surface must run.
            assert!(
                kinds.contains(&ActivationPhaseKind::ResolveSystemBuildToplevel),
                "{}: missing ResolveSystemBuildToplevel",
                algo.name,
            );
            assert!(
                kinds.contains(&ActivationPhaseKind::ComposeActivationScript),
                "{}: missing ComposeActivationScript",
                algo.name,
            );
            assert!(
                kinds.contains(&ActivationPhaseKind::WriteActivationDerivation),
                "{}: missing WriteActivationDerivation",
                algo.name,
            );
        }
    }

    #[test]
    fn darwin_uses_launchd_not_systemd() {
        let algos = load_canonical().unwrap();
        let darwin = algos
            .iter()
            .find(|a| a.target == ActivationTarget::Darwin)
            .expect("darwin algo must exist");
        let kinds: Vec<ActivationPhaseKind> =
            darwin.phases.iter().map(|p| p.kind).collect();
        assert!(kinds.contains(&ActivationPhaseKind::GenerateLaunchdPlists));
        assert!(!kinds.contains(&ActivationPhaseKind::GenerateSystemdUnits));
    }

    #[test]
    fn nixos_uses_systemd_not_launchd() {
        let algos = load_canonical().unwrap();
        let nixos = algos
            .iter()
            .find(|a| a.target == ActivationTarget::NixOS)
            .expect("nixos algo must exist");
        let kinds: Vec<ActivationPhaseKind> =
            nixos.phases.iter().map(|p| p.kind).collect();
        assert!(kinds.contains(&ActivationPhaseKind::GenerateSystemdUnits));
        assert!(!kinds.contains(&ActivationPhaseKind::GenerateLaunchdPlists));
    }

    #[test]
    fn apply_is_a_typed_not_yet() {
        let algo = ActivationScriptAlgorithm {
            name: "test".into(),
            target: ActivationTarget::Darwin,
            phases: vec![],
        };
        let err = apply(
            &algo,
            ActivationArgs {
                toplevel_path: "/nix/store/zzz-toplevel".into(),
                host: "cid".into(),
                user: "drzzln".into(),
            },
        )
        .expect_err("apply must return error until M3 lands");
        match err {
            SpecError::Interp { phase, message } => {
                assert_eq!(phase, "activation");
                assert!(message.contains("not yet landed"));
            }
            _ => panic!("expected SpecError::Interp, got {err:?}"),
        }
    }
}
