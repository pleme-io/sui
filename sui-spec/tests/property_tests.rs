//! Property-based tests for the typed ParityCheck contract.
//!
//! Each property covers an invariant the typed border MUST hold for
//! every input, not just hand-picked examples.  Per the prime
//! directive: stand on solid abstractions — properties prove the
//! abstraction stays sound under any input shape, the way unit
//! tests can't.

use std::path::{Path, PathBuf};

use proptest::prelude::*;

use sui_spec::cli::{nix_cli, sui_cli, NIX_EXPERIMENTAL_FEATURES};
use sui_spec::exec::command_argv;
use sui_spec::parity::{ProbeContext, TargetOs};
use sui_spec::probe::{Classify, Probe};
use sui_spec::rebuild::{
    HostMode, RebuildCompare, RebuildProbe, RebuildStage, RebuildStageKind, ToplevelTarget,
};

// ── Generators ────────────────────────────────────────────────────

fn arb_classify() -> impl Strategy<Value = Classify> {
    prop_oneof![
        Just(Classify::JsonEqual),
        Just(Classify::AttrNamesEqual),
        Just(Classify::BothAreStorePaths),
    ]
}

fn arb_rebuild_stage_kind() -> impl Strategy<Value = RebuildStageKind> {
    prop_oneof![
        Just(RebuildStageKind::FlakeShowKeys),
        Just(RebuildStageKind::FlakeCheckExit),
        Just(RebuildStageKind::EvalToplevel),
        Just(RebuildStageKind::EvalHomeActivation),
        Just(RebuildStageKind::DryRunClosure),
        Just(RebuildStageKind::InputLockHash),
        Just(RebuildStageKind::ClosureSize),
        Just(RebuildStageKind::ClosureReferenceGraph),
    ]
}

fn arb_target() -> impl Strategy<Value = ToplevelTarget> {
    prop_oneof![Just(ToplevelTarget::Darwin), Just(ToplevelTarget::NixOS)]
}

fn arb_compare() -> impl Strategy<Value = RebuildCompare> {
    prop_oneof![
        Just(RebuildCompare::ExitCode),
        Just(RebuildCompare::JsonEqual),
        Just(RebuildCompare::AttrNamesEqual),
        Just(RebuildCompare::StorePathSet),
        Just(RebuildCompare::IntegerEqual),
        Just(RebuildCompare::GraphIsomorphic),
    ]
}

/// Allowed Nix-expression alphabet — printable ASCII without
/// single/double quotes (which would confuse the surrounding
/// `--expr "..."` quoting at the spawning shell layer).  Realistic
/// probe expressions all live in this charset.
fn arb_nix_expr() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_\\-+/.= ]{1,80}"
}

fn arb_tag() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9-]{0,15}"
}

fn arb_probe() -> impl Strategy<Value = Probe> {
    (arb_nix_expr(), arb_classify(), prop::collection::vec(arb_tag(), 0..5))
        .prop_map(|(expr_body, classify, tags)| {
            // Every probe must contain $FLAKE — the corpus contract.
            let expr = format!("let _ = \"$FLAKE\"; in {expr_body}");
            Probe { name: "p".into(), expr, classify, tags }
        })
}

fn arb_rebuild_probe() -> impl Strategy<Value = RebuildProbe> {
    (
        arb_rebuild_stage_kind(),
        arb_compare(),
        prop::collection::vec(arb_tag(), 0..5),
        prop::option::weighted(0.5, arb_target()),
    )
        .prop_map(|(kind, compare, tags, target)| {
            RebuildProbe {
                name: "r".into(),
                host_mode: HostMode::Current,
                host_literal: None,
                stage: RebuildStage { kind, target, user: None, input: None },
                compare,
                tags,
            }
        })
}

fn ctx() -> ProbeContext {
    ProbeContext {
        flake_path: PathBuf::from("/tmp/fixture-flake"),
        flake_label: "fixture-flake".into(),
        host: "cid".into(),
        system: "aarch64-darwin".into(),
        user: "drzzln".into(),
        os: TargetOs::Darwin,
    }
}

fn sui_bin() -> &'static Path { Path::new("/usr/local/bin/sui") }
fn nix_bin() -> &'static Path { Path::new("/usr/local/bin/nix") }

// ── Properties on Probe ───────────────────────────────────────────

proptest! {
    /// Any Probe's sui invocation always uses the sui binary.
    #[test]
    fn probe_sui_argv0_is_sui_bin(p in arb_probe()) {
        use sui_spec::parity::ParityCheck;
        let cmd = p.sui_invocation(&ctx(), sui_bin());
        let argv = command_argv(&cmd);
        prop_assert_eq!(argv[0].as_str(), "/usr/local/bin/sui");
    }

    /// Any Probe's nix invocation includes the experimental-features
    /// incantation — the value comes from the single source of truth.
    #[test]
    fn probe_nix_carries_experimental_features(p in arb_probe()) {
        use sui_spec::parity::ParityCheck;
        let cmd = p.nix_invocation(&ctx(), nix_bin());
        let argv = command_argv(&cmd);
        prop_assert!(argv.iter().any(|a| a == "--extra-experimental-features"));
        prop_assert!(argv.iter().any(|a| a == NIX_EXPERIMENTAL_FEATURES));
    }

    /// The `$FLAKE` placeholder is always expanded to the real flake
    /// path in both engines' invocations.  (Substitute is non-recursive,
    /// so $HOST inside the body of an expression doesn't expand
    /// further, but $FLAKE is always replaced.)
    #[test]
    fn probe_invocations_expand_flake_placeholder(p in arb_probe()) {
        use sui_spec::parity::ParityCheck;
        let sui_cmd = p.sui_invocation(&ctx(), sui_bin());
        let nix_cmd = p.nix_invocation(&ctx(), nix_bin());
        let sui_argv = command_argv(&sui_cmd);
        let nix_argv = command_argv(&nix_cmd);
        let saw_expansion_in =
            |argv: &[String]| argv.iter().any(|a| a.contains("/tmp/fixture-flake"));
        prop_assert!(saw_expansion_in(&sui_argv),
            "sui argv must expand $FLAKE; got {sui_argv:?}");
        prop_assert!(saw_expansion_in(&nix_argv),
            "nix argv must expand $FLAKE; got {nix_argv:?}");
        // No remaining literal `$FLAKE`.
        prop_assert!(!sui_argv.iter().any(|a| a.contains("$FLAKE")));
        prop_assert!(!nix_argv.iter().any(|a| a.contains("$FLAKE")));
    }
}

// ── Properties on RebuildProbe ────────────────────────────────────

proptest! {
    /// Every RebuildProbe's sui_invocation has the sui binary at argv[0].
    #[test]
    fn rebuild_sui_argv0_is_sui_bin(p in arb_rebuild_probe()) {
        use sui_spec::parity::ParityCheck;
        let cmd = p.sui_invocation(&ctx(), sui_bin());
        let argv = command_argv(&cmd);
        prop_assert_eq!(argv[0].as_str(), "/usr/local/bin/sui");
    }

    /// Every RebuildProbe's nix_invocation carries experimental
    /// features — exhaustive cover of every RebuildStageKind.  If a
    /// future stage forgets to consume the nix_cli helpers, this
    /// property fires.
    #[test]
    fn rebuild_nix_carries_experimental_features(p in arb_rebuild_probe()) {
        use sui_spec::parity::ParityCheck;
        let cmd = p.nix_invocation(&ctx(), nix_bin());
        let argv = command_argv(&cmd);
        prop_assert!(argv.iter().any(|a| a == "--extra-experimental-features"));
    }

    /// Every RebuildProbe argv contains the flake path or the
    /// installable referencing it.  Stages that don't take a path
    /// must still flow through ctx (they all do today).
    #[test]
    fn rebuild_argv_includes_flake_reference(p in arb_rebuild_probe()) {
        use sui_spec::parity::ParityCheck;
        let sui_cmd = p.sui_invocation(&ctx(), sui_bin());
        let nix_cmd = p.nix_invocation(&ctx(), nix_bin());
        let sui_argv = command_argv(&sui_cmd);
        let nix_argv = command_argv(&nix_cmd);
        let saw_flake = |argv: &[String]| argv.iter().any(|a| a.contains("/tmp/fixture-flake"));
        prop_assert!(saw_flake(&sui_argv), "sui argv missing flake path; got {sui_argv:?}");
        prop_assert!(saw_flake(&nix_argv), "nix argv missing flake path; got {nix_argv:?}");
    }
}

// ── Properties on the cli builder layer directly ─────────────────

proptest! {
    /// The nix_cli builder always includes experimental features.
    /// Stronger than the per-probe property — covers helpers
    /// directly, in case a future consumer constructs Commands
    /// outside a probe context.
    #[test]
    fn nix_cli_eval_expr_carries_experimental_features(expr in arb_nix_expr()) {
        let cmd = nix_cli::eval_expr(nix_bin(), &expr);
        let argv = command_argv(&cmd);
        prop_assert!(argv.iter().any(|a| a == "--extra-experimental-features"));
        prop_assert!(argv.iter().any(|a| a == NIX_EXPERIMENTAL_FEATURES));
        prop_assert!(argv.iter().any(|a| a == &expr));
    }

    #[test]
    fn nix_cli_flake_show_carries_ref_and_json(p in "[a-z0-9/:]{1,40}") {
        let cmd = nix_cli::flake_show(nix_bin(), &p);
        let argv = command_argv(&cmd);
        prop_assert!(argv.iter().any(|a| a == &p));
        prop_assert!(argv.iter().any(|a| a == "--json"));
        prop_assert!(argv.windows(2).any(|w| w == ["flake", "show"]));
    }

    #[test]
    fn sui_cli_symmetric_with_nix_for_flake_show(p in "[a-z0-9/:]{1,40}") {
        let nix_cmd = nix_cli::flake_show(nix_bin(), &p);
        let sui_cmd = sui_cli::flake_show(sui_bin(), &p);
        let nix_argv = command_argv(&nix_cmd);
        let sui_argv = command_argv(&sui_cmd);
        // Both must carry the flake ref + --json.
        for ref_argv in [&nix_argv, &sui_argv] {
            prop_assert!(ref_argv.iter().any(|a| a == &p));
            prop_assert!(ref_argv.iter().any(|a| a == "--json"));
        }
        // Only nix needs experimental features.
        prop_assert!(nix_argv.iter().any(|a| a == "--extra-experimental-features"));
        prop_assert!(!sui_argv.iter().any(|a| a == "--extra-experimental-features"));
    }
}

// ── Properties on ProbeContext::substitute ───────────────────────

proptest! {
    /// Substitute on a template without any placeholders is a no-op.
    #[test]
    fn substitute_no_placeholders_is_identity(
        body in "[a-zA-Z0-9 .]{0,80}",
    ) {
        let ctx = self::ctx();
        let s = ctx.substitute(&body);
        prop_assert_eq!(s, body);
    }

    /// Substitute is idempotent — substituting an already-substituted
    /// string is identity (the result no longer contains the
    /// placeholder syntax).
    #[test]
    fn substitute_is_idempotent(
        body in "[a-zA-Z ]{0,40}",
        which in 0u8..4,
    ) {
        let ctx = self::ctx();
        let template = match which {
            0 => format!("X $FLAKE {body}"),
            1 => format!("$HOST {body}"),
            2 => format!("$SYSTEM {body}"),
            _ => format!("$USER {body}"),
        };
        let once = ctx.substitute(&template);
        let twice = ctx.substitute(&once);
        prop_assert_eq!(once, twice);
    }
}
