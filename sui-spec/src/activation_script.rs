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

// ── Spec interpreter (M3.0 minimal) ────────────────────────────────

use crate::module_system::{Config, NixValue};

/// Inputs to an activation-script algorithm.
pub struct ActivationArgs {
    /// The evaluated module-system config (typically the output of
    /// `module_system::eval_modules` for the operator's host).
    /// Carries every option's resolved value path-keyed.
    pub config: Config,
    /// Hostname the activation will install on (`hostname -s`).
    pub host: String,
    /// Username for HomeManager target; ignored for NixOS/Darwin.
    pub user: String,
    /// Path to the typed system-build-toplevel store entry.
    /// (Sui-build's derivation realisation produces this for M3.1;
    /// M3.0 callers pass a placeholder.)
    pub toplevel_path: String,
}

/// Result of an activation-script run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationOutcome {
    /// The composed activation script text.  In production this is
    /// written to a `pkgs.runCommand` derivation; M3.0 returns it
    /// as a plain String for testability.
    pub script_text: String,
    /// Per-phase artifact paths produced during the run.  E.g.
    /// `{"plists": "/nix/store/abc-launchd-plists", "etc": "..."}`.
    pub artifacts: std::collections::BTreeMap<String, String>,
    /// Target the script is for — mirrors the algorithm's target.
    pub target: ActivationTarget,
}

/// Apply the activation algorithm.  M3.0 implementation: walks the
/// authored phase pipeline, dispatching each phase to a small text-
/// generation routine.  No actual filesystem writes — output is
/// the activation script TEXT, ready to be written to a derivation.
/// (M3.1 + sui-build will materialise the derivation.)
///
/// # Errors
///
/// - `SpecError::Interp { phase: "<phase-name>" }` for any phase
///   that needs the M3.1 derivation-build bridge (today: only
///   `WriteActivationDerivation` and `AttestClosureToChain`).
pub fn apply(
    algo: &ActivationScriptAlgorithm,
    args: &ActivationArgs,
) -> Result<ActivationOutcome, SpecError> {
    let mut artifacts: std::collections::BTreeMap<String, String> = Default::default();
    let mut script_lines: Vec<String> = Vec::new();

    // Shebang + header common to every target.
    script_lines.push("#!/bin/sh".into());
    script_lines.push(format!(
        "# sui-spec activation script for {:?} on `{}`",
        algo.target, args.host,
    ));
    script_lines.push("set -eu".into());
    script_lines.push(String::new());

    for phase in &algo.phases {
        match phase.kind {
            ActivationPhaseKind::ResolveSystemBuildToplevel => {
                script_lines.push(format!(
                    "# resolve toplevel → {}",
                    args.toplevel_path,
                ));
                artifacts.insert("toplevel".into(), args.toplevel_path.clone());
            }
            ActivationPhaseKind::GenerateSystemdUnits => {
                // Only meaningful for NixOS / HomeManager (linux user).
                if algo.target == ActivationTarget::Darwin {
                    continue;
                }
                let units = list_unit_paths(&args.config);
                script_lines.push("# systemd units:".into());
                for u in &units {
                    script_lines.push(format!("#   {u}"));
                }
                artifacts.insert("systemd-units".into(),
                    format!("/nix/store/zzz-systemd-units-{}", args.host));
                script_lines.push(format!(
                    "systemctl daemon-reload  # {} units",
                    units.len(),
                ));
            }
            ActivationPhaseKind::GenerateLaunchdPlists => {
                if algo.target == ActivationTarget::NixOS {
                    continue;
                }
                let plists = list_launchd_paths(&args.config);
                script_lines.push("# launchd plists:".into());
                for p in &plists {
                    script_lines.push(format!("#   {p}"));
                }
                artifacts.insert("launchd-plists".into(),
                    format!("/nix/store/zzz-launchd-{}", args.host));
            }
            ActivationPhaseKind::GenerateEtcSymlinks => {
                if algo.target == ActivationTarget::HomeManager {
                    continue;
                }
                let entries = list_etc_entries(&args.config);
                script_lines.push("# /etc symlink farm:".into());
                for (target, source) in &entries {
                    script_lines.push(format!("#   /etc/{target} → {source}"));
                }
                artifacts.insert("etc-farm".into(),
                    format!("/nix/store/zzz-etc-{}", args.host));
            }
            ActivationPhaseKind::ResolveSecretRefs => {
                let refs = list_secret_refs(&args.config);
                script_lines.push(format!(
                    "# {} secret refs resolved (handled out-of-band by cofre)",
                    refs.len(),
                ));
            }
            ActivationPhaseKind::ComposeActivationScript => {
                script_lines.push(String::new());
                script_lines.push("# main activation".into());
                script_lines.push(format!(
                    "echo \"activating generation for {}\" >&2",
                    args.host,
                ));
            }
            ActivationPhaseKind::WriteActivationDerivation => {
                // M3.1 — actual derivation write.  For M3.0 we record
                // a placeholder path.
                artifacts.insert("activation-drv".into(),
                    format!("/nix/store/zzz-activation-{}.drv", args.host));
            }
            ActivationPhaseKind::AttestClosureToChain => {
                // Optional — operator opts in via the algorithm
                // variant.  M3.0 records the request; M3.x lands
                // the tameshi OutcomeChain bridge.
                artifacts.insert("attest-request".into(),
                    format!("pending:{}", args.host));
            }
        }
    }

    Ok(ActivationOutcome {
        script_text: script_lines.join("\n"),
        artifacts,
        target: algo.target,
    })
}

// ── Config inspection helpers ──────────────────────────────────────

fn list_unit_paths(config: &Config) -> Vec<String> {
    // Convention: any path under `systemd.services.<name>` is a
    // unit.  cppnix uses the same path structure.
    let mut out: Vec<String> = config
        .keys()
        .filter(|k| k.starts_with("systemd.services."))
        .cloned()
        .collect();
    out.sort();
    out
}

fn list_launchd_paths(config: &Config) -> Vec<String> {
    // Convention: `launchd.daemons.<name>` for system daemons,
    // `launchd.user.agents.<name>` for user agents.
    let mut out: Vec<String> = config
        .keys()
        .filter(|k| k.starts_with("launchd.daemons.")
            || k.starts_with("launchd.user.agents."))
        .cloned()
        .collect();
    out.sort();
    out
}

fn list_etc_entries(config: &Config) -> Vec<(String, String)> {
    // Convention: `environment.etc.<name>` declares an /etc entry.
    // Value is typically `{ text = "..."; source = path; }`.
    let mut out: Vec<(String, String)> = Vec::new();
    for (path, value) in config {
        let Some(rest) = path.strip_prefix("environment.etc.") else { continue; };
        let source = match value {
            NixValue::String(s) => s.clone(),
            NixValue::Object(o) => o
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or("<inline>")
                .into(),
            _ => "<unknown>".into(),
        };
        out.push((rest.into(), source));
    }
    out.sort();
    out
}

fn list_secret_refs(config: &Config) -> Vec<String> {
    // Convention: any value that's `{ __secretRef = "..." }` is a
    // typed secret reference.  cofre materialises these at
    // activation time.
    let mut out: Vec<String> = Vec::new();
    for (path, value) in config {
        if let NixValue::Object(o) = value {
            if o.contains_key("__secretRef") {
                out.push(path.clone());
            }
        }
    }
    out
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

    // ── M3.0 interpreter tests ─────────────────────────────────

    fn empty_args(host: &str, target: ActivationTarget) -> ActivationArgs {
        let _ = target;
        ActivationArgs {
            config: Config::new(),
            host: host.into(),
            user: "drzzln".into(),
            toplevel_path: format!("/nix/store/zzz-toplevel-{host}"),
        }
    }

    #[test]
    fn empty_darwin_activation_produces_valid_shebang() {
        let algo = load_canonical().unwrap()
            .into_iter()
            .find(|a| a.target == ActivationTarget::Darwin)
            .unwrap();
        let outcome = apply(&algo, &empty_args("cid", ActivationTarget::Darwin)).unwrap();
        assert!(outcome.script_text.starts_with("#!/bin/sh"));
        assert!(outcome.script_text.contains("set -eu"));
        assert_eq!(outcome.target, ActivationTarget::Darwin);
        // Darwin must record launchd-plists artifact, NOT systemd-units.
        assert!(outcome.artifacts.contains_key("launchd-plists"));
        assert!(!outcome.artifacts.contains_key("systemd-units"));
    }

    #[test]
    fn nixos_activation_records_systemd_units_not_launchd() {
        let algo = load_canonical().unwrap()
            .into_iter()
            .find(|a| a.target == ActivationTarget::NixOS)
            .unwrap();
        let outcome = apply(&algo, &empty_args("rio", ActivationTarget::NixOS)).unwrap();
        assert!(outcome.artifacts.contains_key("systemd-units"));
        assert!(!outcome.artifacts.contains_key("launchd-plists"));
    }

    #[test]
    fn home_manager_activation_skips_etc_farm() {
        let algo = load_canonical().unwrap()
            .into_iter()
            .find(|a| a.target == ActivationTarget::HomeManager)
            .unwrap();
        let outcome = apply(&algo, &empty_args("cid", ActivationTarget::HomeManager)).unwrap();
        // HomeManager doesn't write /etc.
        assert!(!outcome.artifacts.contains_key("etc-farm"));
    }

    #[test]
    fn config_with_systemd_units_surfaces_them() {
        let algo = load_canonical().unwrap()
            .into_iter()
            .find(|a| a.target == ActivationTarget::NixOS)
            .unwrap();
        let mut args = empty_args("rio", ActivationTarget::NixOS);
        args.config.insert(
            "systemd.services.nginx".into(),
            serde_json::json!({"description": "web"}),
        );
        args.config.insert(
            "systemd.services.postgres".into(),
            serde_json::json!({"description": "db"}),
        );
        let outcome = apply(&algo, &args).unwrap();
        assert!(outcome.script_text.contains("systemd.services.nginx"));
        assert!(outcome.script_text.contains("systemd.services.postgres"));
        assert!(outcome.script_text.contains("2 units"));
    }

    #[test]
    fn etc_entries_render_into_script() {
        let algo = load_canonical().unwrap()
            .into_iter()
            .find(|a| a.target == ActivationTarget::NixOS)
            .unwrap();
        let mut args = empty_args("rio", ActivationTarget::NixOS);
        args.config.insert(
            "environment.etc.nixos/configuration.nix".into(),
            serde_json::json!({"source": "/nix/store/xyz-config"}),
        );
        let outcome = apply(&algo, &args).unwrap();
        assert!(outcome.script_text.contains("nixos/configuration.nix"));
        assert!(outcome.script_text.contains("/nix/store/xyz-config"));
    }

    #[test]
    fn secret_refs_counted_in_script() {
        let algo = load_canonical().unwrap()
            .into_iter()
            .find(|a| a.target == ActivationTarget::NixOS)
            .unwrap();
        let mut args = empty_args("rio", ActivationTarget::NixOS);
        args.config.insert(
            "services.foo.password".into(),
            serde_json::json!({"__secretRef": "akeyless://prod/db-password"}),
        );
        let outcome = apply(&algo, &args).unwrap();
        assert!(outcome.script_text.contains("1 secret refs"));
    }

    /// The headline chain test: module_system::eval_modules feeds
    /// activation_script::apply.  Proves the substrate primitives
    /// compose end-to-end.
    #[test]
    fn cross_domain_chain_module_system_then_activation() {
        use crate::module_system::{
            eval_modules, Module, OptionDecl, Definition, NixValue,
        };
        use crate::module_system as ms;

        // Build a trivial NixOS-shaped module with one systemd unit.
        let mut module = Module::default();
        module.options.insert(
            "systemd.services.nginx".into(),
            OptionDecl {
                type_name: "attrsOf".into(),
                ..Default::default()
            },
        );
        module.config.push(Definition {
            path: "systemd.services.nginx".into(),
            value: serde_json::json!({"description": "test"}),
            priority: 100,
            cond: None,
        });

        // Run M2.1 eval_modules.
        let registry = crate::module_system::load_canonical().unwrap().types;
        let config = eval_modules(&[module], &registry).unwrap();
        let _ = ms::NixValue::Null; // silence unused-import

        // Feed the resulting Config into M3.0 activation_script.
        let algo = load_canonical().unwrap()
            .into_iter()
            .find(|a| a.target == ActivationTarget::NixOS)
            .unwrap();
        let outcome = apply(&algo, &ActivationArgs {
            config,
            host: "rio".into(),
            user: "drzzln".into(),
            toplevel_path: "/nix/store/zzz-toplevel-rio".into(),
        }).unwrap();

        // Activation script must reference the systemd unit we
        // configured.
        assert!(outcome.script_text.contains("systemd.services.nginx"),
            "activation script must list the configured unit; got: {}",
            outcome.script_text,
        );
        let _ = NixValue::Null;
    }
}
