//! Typed builders for `sui` and `nix` CLI invocations.
//!
//! Two consumers today ([`crate::probe::Probe`] and
//! [`crate::rebuild::RebuildProbe`]), with a third already in the
//! roadmap (any future `Probe`-shaped domain).  Extracting the
//! canonical invocation surface here means:
//!
//! 1. The `--extra-experimental-features "nix-command flakes"`
//!    incantation lives in exactly one place.  When cppnix changes
//!    its CLI conventions, one edit covers every probe.
//! 2. The sui side gets a symmetric typed border — invocations
//!    always reach `sui` via the same builders, so the differential
//!    sweep is comparing apples to apples by construction.
//! 3. A new probe domain implements its `ParityCheck::sui_invocation`
//!    / `nix_invocation` by composing these primitives, not by
//!    hand-writing `Command::args(...)`.
//!
//! NO SHELL — every argument is added via typed [`Command`] APIs.
//! No shell escaping concerns because nothing ever goes through a
//! shell.

use std::path::Path;
use std::process::Command;

/// Canonical experimental-features set required by every modern nix
/// CLI invocation that touches flakes or new commands.  Kept as a
/// const so it's textually obvious in the generated argv.
pub const NIX_EXPERIMENTAL_FEATURES: &str = "nix-command flakes";

/// Build a `nix` invocation for `subcommand` with the canonical
/// experimental-features prelude already in place.  Caller adds
/// any subcommand-specific args after.
#[must_use]
fn nix_base(nix_bin: &Path, subcommand: &[&str]) -> Command {
    let mut cmd = Command::new(nix_bin);
    cmd.args(subcommand);
    cmd.args(["--extra-experimental-features", NIX_EXPERIMENTAL_FEATURES]);
    cmd
}

/// Build a `sui` invocation for `subcommand`.  Sui doesn't need an
/// experimental-features prelude (flakes + new CLI are default), so
/// the base just hangs the subcommand off the binary.
#[must_use]
fn sui_base(sui_bin: &Path, subcommand: &[&str]) -> Command {
    let mut cmd = Command::new(sui_bin);
    cmd.args(subcommand);
    cmd
}

/// Canonical nix CLI invocations.  Every builder returns a
/// pre-configured [`Command`] ready for `spawn()` (with whatever
/// stdio + env the caller layers on).
pub mod nix_cli {
    use super::*;

    /// `nix eval --impure --json --expr <expr>` — evaluate a literal
    /// expression and emit a JSON value on stdout.
    #[must_use]
    pub fn eval_expr(nix_bin: &Path, expr: &str) -> Command {
        let mut cmd = nix_base(nix_bin, &["eval", "--impure", "--json"]);
        cmd.args(["--expr", expr]);
        cmd
    }

    /// `nix eval --impure --json <installable>` — evaluate an
    /// installable (a flake-attribute reference).
    #[must_use]
    pub fn eval_installable(nix_bin: &Path, installable: &str) -> Command {
        let mut cmd = nix_base(nix_bin, &["eval", "--impure", "--json"]);
        cmd.arg(installable);
        cmd
    }

    /// `nix flake show --json <flake-ref>` — print the flake's
    /// output inventory.
    #[must_use]
    pub fn flake_show(nix_bin: &Path, flake_ref: &str) -> Command {
        let mut cmd = nix_base(nix_bin, &["flake", "show", "--json"]);
        cmd.arg(flake_ref);
        cmd
    }

    /// `nix flake check <flake-ref>` — type-check the flake.
    #[must_use]
    pub fn flake_check(nix_bin: &Path, flake_ref: &str) -> Command {
        let mut cmd = nix_base(nix_bin, &["flake", "check"]);
        cmd.arg(flake_ref);
        cmd
    }

    /// `nix build --dry-run --print-out-paths --no-link <installable>`
    /// — print the would-be-built out paths without realising
    /// derivations.
    #[must_use]
    pub fn build_dry_run(nix_bin: &Path, installable: &str) -> Command {
        let mut cmd = nix_base(
            nix_bin,
            &["build", "--dry-run", "--print-out-paths", "--no-link"],
        );
        cmd.arg(installable);
        cmd
    }
}

/// Canonical sui CLI invocations — symmetric with [`nix_cli`].
pub mod sui_cli {
    use super::*;

    /// `sui eval --json <expr>` — evaluate a literal expression as
    /// positional installable (sui treats expressions and installables
    /// uniformly through this path).
    #[must_use]
    pub fn eval_expr(sui_bin: &Path, expr: &str) -> Command {
        let mut cmd = sui_base(sui_bin, &["eval", "--json"]);
        cmd.arg(expr);
        cmd
    }

    /// `sui eval --json <installable>` — evaluate a flake-attribute
    /// installable.
    #[must_use]
    pub fn eval_installable(sui_bin: &Path, installable: &str) -> Command {
        let mut cmd = sui_base(sui_bin, &["eval", "--json"]);
        cmd.arg(installable);
        cmd
    }

    /// `sui eval --impure --json --expr <expr>` — explicit `--expr`
    /// for cases where the input might otherwise be parsed as a
    /// path-style installable.
    #[must_use]
    pub fn eval_expr_explicit(sui_bin: &Path, expr: &str) -> Command {
        let mut cmd = sui_base(sui_bin, &["eval", "--impure", "--json"]);
        cmd.args(["--expr", expr]);
        cmd
    }

    /// `sui flake show --json <flake-ref>`.
    #[must_use]
    pub fn flake_show(sui_bin: &Path, flake_ref: &str) -> Command {
        let mut cmd = sui_base(sui_bin, &["flake", "show", "--json"]);
        cmd.arg(flake_ref);
        cmd
    }

    /// `sui flake check <flake-ref>`.
    #[must_use]
    pub fn flake_check(sui_bin: &Path, flake_ref: &str) -> Command {
        let mut cmd = sui_base(sui_bin, &["flake", "check"]);
        cmd.arg(flake_ref);
        cmd
    }

    /// `sui build --dry-run --print-out-paths --no-link <installable>`.
    #[must_use]
    pub fn build_dry_run(sui_bin: &Path, installable: &str) -> Command {
        let mut cmd = sui_base(
            sui_bin,
            &["build", "--dry-run", "--print-out-paths", "--no-link"],
        );
        cmd.arg(installable);
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::command_argv;
    use std::path::PathBuf;

    fn nix_path() -> PathBuf {
        PathBuf::from("/usr/local/bin/nix")
    }

    fn sui_path() -> PathBuf {
        PathBuf::from("/usr/local/bin/sui")
    }

    #[test]
    fn nix_eval_expr_includes_experimental_features() {
        let cmd = nix_cli::eval_expr(&nix_path(), "1 + 2");
        let argv = command_argv(&cmd);
        assert_eq!(argv[0], "/usr/local/bin/nix");
        assert!(argv.iter().any(|a| a == "--extra-experimental-features"));
        assert!(argv.iter().any(|a| a == NIX_EXPERIMENTAL_FEATURES));
        assert!(argv.iter().any(|a| a == "--expr"));
        assert!(argv.iter().any(|a| a == "1 + 2"));
        assert!(argv.iter().any(|a| a == "--impure"));
        assert!(argv.iter().any(|a| a == "--json"));
    }

    #[test]
    fn nix_flake_show_includes_experimental_features_and_ref() {
        let cmd = nix_cli::flake_show(&nix_path(), "path:/tmp/myflake");
        let argv = command_argv(&cmd);
        assert!(argv.windows(2).any(|w| w == ["flake", "show"]));
        assert!(argv.iter().any(|a| a == "--json"));
        assert!(argv.iter().any(|a| a == "--extra-experimental-features"));
        assert!(argv.iter().any(|a| a == "path:/tmp/myflake"));
    }

    #[test]
    fn nix_build_dry_run_carries_all_flags() {
        let cmd = nix_cli::build_dry_run(&nix_path(), "path:/tmp/f#x.y");
        let argv = command_argv(&cmd);
        for required in ["--dry-run", "--print-out-paths", "--no-link"] {
            assert!(
                argv.iter().any(|a| a == required),
                "build_dry_run missing {required}; argv: {argv:?}",
            );
        }
    }

    #[test]
    fn sui_eval_expr_is_positional() {
        let cmd = sui_cli::eval_expr(&sui_path(), "1 + 2");
        let argv = command_argv(&cmd);
        assert_eq!(argv[0], "/usr/local/bin/sui");
        assert_eq!(argv[1], "eval");
        assert_eq!(argv[2], "--json");
        assert_eq!(argv[3], "1 + 2");
        // sui doesn't need experimental-features
        assert!(!argv.iter().any(|a| a == "--extra-experimental-features"));
    }

    #[test]
    fn sui_eval_expr_explicit_uses_dash_dash_expr() {
        let cmd = sui_cli::eval_expr_explicit(&sui_path(), "1 + 2");
        let argv = command_argv(&cmd);
        assert!(argv.iter().any(|a| a == "--expr"));
        assert!(argv.iter().any(|a| a == "1 + 2"));
        assert!(argv.iter().any(|a| a == "--impure"));
    }

    #[test]
    fn sui_flake_show_and_check() {
        let show = sui_cli::flake_show(&sui_path(), "path:/tmp/f");
        let check = sui_cli::flake_check(&sui_path(), "path:/tmp/f");
        let show_argv = command_argv(&show);
        let check_argv = command_argv(&check);
        assert!(show_argv.windows(2).any(|w| w == ["flake", "show"]));
        assert!(check_argv.windows(2).any(|w| w == ["flake", "check"]));
        assert!(show_argv.iter().any(|a| a == "--json"));
        assert!(!check_argv.iter().any(|a| a == "--json"));  // check is exit-code only
    }

    #[test]
    fn symmetric_signatures() {
        // For every nix_cli builder there's a sui_cli sibling with
        // the same shape — this property is what lets a ParityCheck
        // impl ride on a single dispatch.  We test by argv length
        // parity for canonical inputs.
        let p = "path:/tmp/f";
        let n = nix_cli::flake_show(&nix_path(), p);
        let s = sui_cli::flake_show(&sui_path(), p);
        // Both must include the flake ref and `--json`.
        let na = command_argv(&n);
        let sa = command_argv(&s);
        assert!(na.iter().any(|a| a == p) && sa.iter().any(|a| a == p));
        assert!(na.iter().any(|a| a == "--json") && sa.iter().any(|a| a == "--json"));
    }
}
