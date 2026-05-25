//! `argv[0]` dispatch — when the `sui` binary is symlinked to a
//! legacy `nix-*` name (`nix-build`, `nix-store`, `nix-env`, …), the
//! binary detects the invoked name and rewrites the legacy CLI
//! surface into the modern `sui <subcommand>` form.
//!
//! Lives in its own module so the typed translation table doesn't
//! sprawl into `main.rs`.  Each [`LegacyCmd`] variant declares its
//! sui equivalent; [`translate_legacy_argv`] is exhaustive over the
//! enum so the compiler enforces coverage when a variant is added.
//!
//! ## Why a typed enum (and not a string-table)
//!
//! - the variant set is closed (nix's legacy surface doesn't grow)
//! - per-command arg rewrites are different shapes (`nix-build -A`
//!   vs `nix-store --realise` vs `nix-env --switch-generation` are
//!   not unified by a single rewrite); a per-variant function keeps
//!   each translation typed and tested in isolation
//! - compiler-enforced exhaustiveness means a new variant lights up
//!   every missing arm
//!
//! ## Symlink farm
//!
//! `flake.nix` ships a `sui-as-nix` package output that
//! `symlinkJoin`s the sui binary with one symlink per
//! [`LegacyCmd::all_names`] entry pointing at `bin/sui`.  Operators
//! set `nix.package = inputs.sui.packages.${system}.sui-as-nix;` (or
//! import `nixosModules.default-as-nix`) to drop sui into every
//! cppnix call site.

use std::path::Path;

/// One legacy `nix-*` CLI entry.  Names are stable (`nix-build`,
/// `nix-store`, …) and known up front.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyCmd {
    Build,
    Store,
    Env,
    Shell,
    Instantiate,
    CollectGarbage,
    Hash,
    CopyClosure,
    Channel,
    Daemon,
    PrefetchUrl,
}

impl LegacyCmd {
    /// Stable basename for each variant.  Used both for argv[0]
    /// detection and to emit the symlink-farm in `flake.nix`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Build           => "nix-build",
            Self::Store           => "nix-store",
            Self::Env             => "nix-env",
            Self::Shell           => "nix-shell",
            Self::Instantiate     => "nix-instantiate",
            Self::CollectGarbage  => "nix-collect-garbage",
            Self::Hash            => "nix-hash",
            Self::CopyClosure     => "nix-copy-closure",
            Self::Channel         => "nix-channel",
            Self::Daemon          => "nix-daemon",
            Self::PrefetchUrl     => "nix-prefetch-url",
        }
    }

    /// Every legacy variant (for the symlink farm).
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Build, Self::Store, Self::Env, Self::Shell,
            Self::Instantiate, Self::CollectGarbage, Self::Hash,
            Self::CopyClosure, Self::Channel, Self::Daemon,
            Self::PrefetchUrl,
        ]
    }

    /// Every legacy basename string (for emitting the symlink-farm
    /// list in `flake.nix` / `nixosModules.default-as-nix`).
    #[must_use]
    pub fn all_names() -> Vec<&'static str> {
        Self::all().iter().map(|c| c.name()).collect()
    }

    /// Detect the currently-invoked legacy basename, if any.
    /// Reads `argv[0]`, strips any directory, and matches.
    #[must_use]
    pub fn detect() -> Option<Self> {
        let arg0 = std::env::args().next()?;
        let name = Path::new(&arg0).file_name()?.to_str()?;
        Self::from_basename(name)
    }

    /// Match a basename string against the variant table.
    #[must_use]
    pub fn from_basename(name: &str) -> Option<Self> {
        match name {
            "nix-build"           => Some(Self::Build),
            "nix-store"           => Some(Self::Store),
            "nix-env"             => Some(Self::Env),
            "nix-shell"           => Some(Self::Shell),
            "nix-instantiate"     => Some(Self::Instantiate),
            "nix-collect-garbage" => Some(Self::CollectGarbage),
            "nix-hash"            => Some(Self::Hash),
            "nix-copy-closure"    => Some(Self::CopyClosure),
            "nix-channel"         => Some(Self::Channel),
            "nix-daemon"          => Some(Self::Daemon),
            "nix-prefetch-url"    => Some(Self::PrefetchUrl),
            _ => None,
        }
    }
}

/// Translate the legacy argv (post-`argv[0]`) into a modern sui
/// argv, suitable to feed `Cli::parse_from(["sui", …translated])`.
///
/// Each per-command translator handles the common-path flags; rarer
/// flags are passed through verbatim (so sui's own clap parser is the
/// final source of truth for what's accepted).
#[must_use]
pub fn translate_legacy_argv(cmd: LegacyCmd, args: &[String]) -> Vec<String> {
    match cmd {
        LegacyCmd::Build           => translate_build(args),
        LegacyCmd::Store           => translate_store(args),
        LegacyCmd::Env             => translate_env(args),
        LegacyCmd::Shell           => translate_shell(args),
        LegacyCmd::Instantiate     => translate_instantiate(args),
        LegacyCmd::CollectGarbage  => translate_collect_garbage(args),
        LegacyCmd::Hash            => translate_hash(args),
        LegacyCmd::CopyClosure     => prefix("copy", args),
        LegacyCmd::Channel         => translate_channel(args),
        LegacyCmd::Daemon          => prefix("daemon", args),
        LegacyCmd::PrefetchUrl     => prefix_with_subs(&["store", "prefetch-file"], args),
    }
}

/// `nix-build [-A attr] [path] [flags]` → `sui build <path>#<attr> [flags]`.
fn translate_build(args: &[String]) -> Vec<String> {
    let mut attr: Option<String> = None;
    let mut path: Option<String> = None;
    let mut passthrough: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-A" | "--attr" if i + 1 < args.len() => {
                attr = Some(args[i + 1].clone());
                i += 1;
            }
            "--no-out-link" => passthrough.push("--no-link".into()),
            s if !s.starts_with('-') && path.is_none() => path = Some(s.to_string()),
            other => passthrough.push(other.to_string()),
        }
        i += 1;
    }
    let installable = build_installable(path.as_deref(), attr.as_deref());
    let mut out = vec!["build".to_string(), installable];
    out.extend(passthrough);
    out
}

fn build_installable(path: Option<&str>, attr: Option<&str>) -> String {
    match (path, attr) {
        (Some(p), Some(a)) => format!("{p}#{a}"),
        (Some(p), None)    => p.to_string(),
        (None,    Some(a)) => format!(".#{a}"),
        (None,    None)    => ".".to_string(),
    }
}

/// `nix-store` → `sui store <op>` (op derived from flag).
fn translate_store(args: &[String]) -> Vec<String> {
    let (op, rest) = strip_one_op(args, &[
        ("--realise",   "realise"),
        ("-r",          "realise"),
        ("--query",     "info"),
        ("-q",          "info"),
        ("--gc",        "gc"),
        ("--optimise",  "optimise"),
        ("--verify",    "verify"),
        ("--delete",    "delete"),
        ("--add",       "add"),
        ("--dump",      "dump-path"),
        ("--export",    "export"),
        ("--import",    "import"),
    ]);
    let mut out = vec!["store".to_string()];
    if let Some(op_name) = op {
        out.push(op_name.into());
    }
    out.extend(rest);
    out
}

/// `nix-env` → `sui profile <op>`.  The set of flags `nixos-rebuild`
/// actually invokes is small (`--switch-generation`, `--list-generations`,
/// `--set`, `--profile`, `-p`) so we cover those typed; others pass through.
fn translate_env(args: &[String]) -> Vec<String> {
    let (op, rest) = strip_one_op(args, &[
        ("--switch-generation",   "switch-generation"),
        ("-G",                    "switch-generation"),
        ("--list-generations",    "history"),
        ("--set",                 "set"),
        ("--install",             "install"),
        ("-i",                    "install"),
        ("--uninstall",           "remove"),
        ("-e",                    "remove"),
        ("--upgrade",             "upgrade"),
        ("-u",                    "upgrade"),
        ("--query",               "list"),
        ("-q",                    "list"),
        ("--rollback",            "rollback"),
        ("--delete-generations",  "wipe-history"),
    ]);
    let mut out = vec!["profile".to_string()];
    if let Some(op_name) = op {
        out.push(op_name.into());
    }
    out.extend(rest);
    out
}

/// `nix-shell` → `sui develop`.  `-p PKG` becomes part of the
/// installable (`develop nixpkgs#PKG`); `--run` / `--command` map to
/// `sui develop --command`.
fn translate_shell(args: &[String]) -> Vec<String> {
    let mut out = vec!["develop".to_string()];
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-p" | "--packages" if i + 1 < args.len() => {
                out.push(format!("nixpkgs#{}", args[i + 1]));
                i += 1;
            }
            "--run" | "--command" if i + 1 < args.len() => {
                out.push("--command".into());
                out.push(args[i + 1].clone());
                i += 1;
            }
            other => out.push(other.to_string()),
        }
        i += 1;
    }
    out
}

/// `nix-instantiate --eval EXPR` → `sui eval EXPR`.
/// Pure-instantiate (without `--eval`) currently has no sui equivalent
/// and is passed through so sui's clap reports a clear NotImplemented.
fn translate_instantiate(args: &[String]) -> Vec<String> {
    let mut out = vec!["eval".to_string()];
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--eval" | "--strict" | "--read-write-mode" => {} // sui eval is always eval+strict
            "-E" | "--expr" if i + 1 < args.len() => {
                out.push("--expr".into());
                out.push(args[i + 1].clone());
                i += 1;
            }
            "--json" => out.push("--json".into()),
            "--raw"  => out.push("--raw".into()),
            other => out.push(other.to_string()),
        }
        i += 1;
    }
    out
}

/// `nix-collect-garbage [-d]` → `sui collect-garbage [-d]`.
fn translate_collect_garbage(args: &[String]) -> Vec<String> {
    let mut out = vec!["collect-garbage".to_string()];
    out.extend(args.iter().cloned());
    out
}

/// `nix-hash` → `sui hash`.
fn translate_hash(args: &[String]) -> Vec<String> {
    let mut out = vec!["hash".to_string()];
    out.extend(args.iter().cloned());
    out
}

/// `nix-channel` → `sui registry` for the operations sui supports;
/// otherwise passed through so sui surfaces a clear error.
fn translate_channel(args: &[String]) -> Vec<String> {
    let (op, rest) = strip_one_op(args, &[
        ("--add",     "add"),
        ("--remove",  "remove"),
        ("--list",    "list"),
    ]);
    let mut out = vec!["registry".to_string()];
    if let Some(op_name) = op {
        out.push(op_name.into());
    }
    out.extend(rest);
    out
}

// ── helpers ─────────────────────────────────────────────────────────

/// Scan `args` for the first matching flag → op pair.  Returns
/// `(op, args minus the matched flag)`.  Used by translators where
/// the legacy CLI dispatches on one flag.
fn strip_one_op(args: &[String], table: &[(&str, &'static str)]) -> (Option<&'static str>, Vec<String>) {
    let mut op: Option<&'static str> = None;
    let mut rest: Vec<String> = Vec::with_capacity(args.len());
    for a in args {
        if op.is_none() {
            if let Some((_, mapped)) = table.iter().find(|(flag, _)| flag == &a.as_str()) {
                op = Some(*mapped);
                continue;
            }
        }
        rest.push(a.clone());
    }
    (op, rest)
}

/// Construct `[head, args...]`.
fn prefix(head: &str, args: &[String]) -> Vec<String> {
    let mut out = vec![head.to_string()];
    out.extend(args.iter().cloned());
    out
}

/// Construct `[head_path..., args...]`.
fn prefix_with_subs(head: &[&str], args: &[String]) -> Vec<String> {
    let mut out: Vec<String> = head.iter().map(|s| (*s).to_string()).collect();
    out.extend(args.iter().cloned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> { v.iter().map(|x| (*x).to_string()).collect() }

    #[test]
    fn detect_known_legacy_basenames() {
        for c in LegacyCmd::all() {
            assert_eq!(LegacyCmd::from_basename(c.name()), Some(*c));
        }
    }

    #[test]
    fn detect_rejects_unknown_basenames() {
        assert_eq!(LegacyCmd::from_basename("sui"), None);
        assert_eq!(LegacyCmd::from_basename("nix"), None);
        assert_eq!(LegacyCmd::from_basename("nixos-rebuild"), None);
        assert_eq!(LegacyCmd::from_basename(""), None);
    }

    #[test]
    fn all_names_round_trip() {
        for name in LegacyCmd::all_names() {
            assert!(LegacyCmd::from_basename(name).is_some(), "{name}");
        }
    }

    #[test]
    fn translate_nix_build_with_attr() {
        let got = translate_legacy_argv(LegacyCmd::Build, &s(&["-A", "hello", "default.nix"]));
        assert_eq!(got, s(&["build", "default.nix#hello"]));
    }

    #[test]
    fn translate_nix_build_default_path_and_attr() {
        let got = translate_legacy_argv(LegacyCmd::Build, &s(&["-A", "hello"]));
        assert_eq!(got, s(&["build", ".#hello"]));
    }

    #[test]
    fn translate_nix_build_path_only() {
        let got = translate_legacy_argv(LegacyCmd::Build, &s(&["default.nix"]));
        assert_eq!(got, s(&["build", "default.nix"]));
    }

    #[test]
    fn translate_nix_build_passthrough_no_out_link() {
        let got = translate_legacy_argv(LegacyCmd::Build, &s(&["--no-out-link", "default.nix"]));
        assert_eq!(got, s(&["build", "default.nix", "--no-link"]));
    }

    #[test]
    fn translate_nix_store_realise() {
        let got = translate_legacy_argv(LegacyCmd::Store, &s(&["--realise", "/nix/store/x.drv"]));
        assert_eq!(got, s(&["store", "realise", "/nix/store/x.drv"]));
    }

    #[test]
    fn translate_nix_store_query_requisites() {
        let got = translate_legacy_argv(LegacyCmd::Store, &s(&["--query", "--requisites", "/nix/store/x"]));
        assert_eq!(got, s(&["store", "info", "--requisites", "/nix/store/x"]));
    }

    #[test]
    fn translate_nix_store_gc() {
        let got = translate_legacy_argv(LegacyCmd::Store, &s(&["--gc"]));
        assert_eq!(got, s(&["store", "gc"]));
    }

    #[test]
    fn translate_nix_env_switch_generation() {
        let got = translate_legacy_argv(LegacyCmd::Env, &s(&["--switch-generation", "42"]));
        assert_eq!(got, s(&["profile", "switch-generation", "42"]));
    }

    #[test]
    fn translate_nix_env_list_generations() {
        let got = translate_legacy_argv(LegacyCmd::Env, &s(&["--list-generations"]));
        assert_eq!(got, s(&["profile", "history"]));
    }

    #[test]
    fn translate_nix_shell_packages() {
        let got = translate_legacy_argv(LegacyCmd::Shell, &s(&["-p", "ripgrep"]));
        assert_eq!(got, s(&["develop", "nixpkgs#ripgrep"]));
    }

    #[test]
    fn translate_nix_shell_run() {
        let got = translate_legacy_argv(LegacyCmd::Shell, &s(&["--run", "echo hi"]));
        assert_eq!(got, s(&["develop", "--command", "echo hi"]));
    }

    #[test]
    fn translate_nix_instantiate_eval_expr() {
        let got = translate_legacy_argv(LegacyCmd::Instantiate, &s(&["--eval", "-E", "1+2", "--json"]));
        assert_eq!(got, s(&["eval", "--expr", "1+2", "--json"]));
    }

    #[test]
    fn translate_nix_collect_garbage_passthrough() {
        let got = translate_legacy_argv(LegacyCmd::CollectGarbage, &s(&["-d"]));
        assert_eq!(got, s(&["collect-garbage", "-d"]));
    }

    #[test]
    fn translate_nix_hash_to_sri() {
        let got = translate_legacy_argv(LegacyCmd::Hash, &s(&["to-sri", "sha256:abc"]));
        assert_eq!(got, s(&["hash", "to-sri", "sha256:abc"]));
    }
}
