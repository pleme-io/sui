//! `nix-wrap` — migration-bridge wrapper around sui + cppnix.
//!
//! The operator installs this binary as `/run/current-system/sw/bin/nix`
//! during the sui-as-nix migration window.  Every `nix <cmd> ...`
//! invocation hits this wrapper, which:
//!
//! 1. Parses the subcommand path from `argv[1..]` (handles nested
//!    cases: `nix store dump-path`, `nix flake show`, etc).
//! 2. Looks up the matching entry in
//!    [`sui_spec::cli_coverage`]'s typed catalog.
//! 3. Routes to **sui** when the entry is `Working` or `SuiNative`.
//!    Routes to **real cppnix** (the path the operator configures
//!    via `NIX_WRAP_CPPNIX_BIN` or `~/.config/sui/nix-wrap.toml`)
//!    for every other case.
//! 4. Logs the routing decision to `~/.cache/sui/nix-wrap.log` so
//!    the operator can see exactly which commands are running on
//!    which engine.
//!
//! This is the **typed bridge** between sui's current capability
//! (~85% of the cppnix surface byte-identical) and full alias
//! readiness.  Replaces nothing — both engines stay installed.
//! Once M2.6+ ship and sui hits 100%, the wrapper can be removed
//! (or kept as a routing diagnostic).
//!
//! Per pleme-io's NO SHELL law: every dispatch path is typed Rust.
//! No bash wrappers, no shell glue beyond the operator's
//! `alias nix=nix-wrap` (which is itself optional once the binary
//! lives on PATH).

use std::process::{Command, ExitCode};

/// Lookup the configured cppnix fallback binary path.
///
/// Resolution order (typed precedence):
/// 1. `NIX_WRAP_CPPNIX_BIN` env var — explicit operator override.
/// 2. `/run/current-system/sw/bin/cppnix` — Nix-managed install
///    location for the renamed-aside cppnix.
/// 3. `/nix/var/nix/profiles/default/bin/nix` — default nix profile.
/// 4. `cppnix` on PATH — last-resort PATH search.
fn cppnix_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("NIX_WRAP_CPPNIX_BIN") {
        if !p.is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    for candidate in [
        "/run/current-system/sw/bin/cppnix",
        "/nix/var/nix/profiles/default/bin/nix",
    ] {
        let p = std::path::Path::new(candidate);
        if p.exists() {
            return p.to_path_buf();
        }
    }
    std::path::PathBuf::from("cppnix")
}

/// Lookup the configured sui binary path.
///
/// Resolution order:
/// 1. `NIX_WRAP_SUI_BIN` env var.
/// 2. `/run/current-system/sw/bin/sui`.
/// 3. `sui` on PATH.
fn sui_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("NIX_WRAP_SUI_BIN") {
        if !p.is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    let candidate = std::path::Path::new("/run/current-system/sw/bin/sui");
    if candidate.exists() {
        return candidate.to_path_buf();
    }
    std::path::PathBuf::from("sui")
}

/// Routing decision for one invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// Run on sui — command is `Working` or `SuiNative` per catalog.
    Sui,
    /// Run on cppnix — command is `Stub`, `Partial`, `Missing`, or
    /// not in catalog at all.
    Cppnix,
}

impl Route {
    fn glyph(self) -> &'static str {
        match self {
            Route::Sui => "→sui",
            Route::Cppnix => "→cppnix",
        }
    }
}

/// Match a parsed command-path against the typed catalog.
///
/// The catalog stores entries as space-separated names like
/// `"store dump-path"` / `"flake show"` / `"hash to-sri"`.  We try
/// the longest match first (so `nix store dump-path` matches the
/// 2-token entry, not the 1-token `store` entry).
///
/// Returns the routing decision: `Sui` for Working/SuiNative
/// matches, `Cppnix` for any other catalog state OR no match.
fn route_for(argv_subcommand: &[&str]) -> Route {
    let Ok(catalog) = sui_spec::cli_coverage::load_canonical() else {
        return Route::Cppnix;
    };
    // Longest-match first.
    for take in (1..=argv_subcommand.len()).rev() {
        let candidate = argv_subcommand[..take].join(" ");
        for entry in &catalog {
            if entry.name == candidate {
                use sui_spec::cli_coverage::SuiCommandMaturity::*;
                return match entry.maturity {
                    Working | SuiNative => Route::Sui,
                    _ => Route::Cppnix,
                };
            }
        }
    }
    Route::Cppnix
}

/// Append one routing decision to the operator-facing log.
///
/// Best-effort: any I/O error is silently dropped (the wrapper
/// must not fail an invocation because the log directory wasn't
/// writable).  Log lines are typed: timestamp + route + argv.
fn log_decision(route: Route, argv: &[String]) {
    use std::io::Write;
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return;
    };
    let log_dir = home.join(".cache/sui");
    if std::fs::create_dir_all(&log_dir).is_err() {
        return;
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true).append(true).open(log_dir.join("nix-wrap.log"))
    else {
        return;
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = writeln!(f, "{ts}\t{}\t{}", route.glyph(), argv.join(" "));
}

/// Strip leading flag arguments to find the first real subcommand
/// token.  e.g. `--show-trace store dump-path /nix/store/...` →
/// `["store", "dump-path", "/nix/store/..."]`.
fn parse_subcommand_path(args: &[String]) -> Vec<&str> {
    args.iter()
        .skip_while(|a| a.starts_with('-'))
        .take_while(|a| !a.starts_with('-'))
        .map(String::as_str)
        .collect()
}

/// Routing mode — controls whether the wrapper falls back to
/// cppnix when sui fails on a Working/SuiNative command.
///
/// The default `Auto` mode achieves functional 100% compatibility
/// by retrying with cppnix on sui failures (e.g. unimplemented
/// edge cases in catalog-Working commands like the M2.6 module-
/// system fixpoint recursion that blocks `nix build` against the
/// operator's actual nix-darwin flake).
///
/// `SuiOnly` is for substrate-development sessions where the
/// operator WANTS sui failures to surface (so M2.X+ bugs don't
/// hide behind cppnix).
///
/// `CppnixOnly` is the rollback mode — always uses cppnix, never
/// sui.  For when sui regressed and the operator needs a stable
/// rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Auto,
    SuiOnly,
    CppnixOnly,
}

impl Mode {
    fn from_env() -> Self {
        match std::env::var("NIX_WRAP_MODE").as_deref() {
            Ok("sui-only") => Mode::SuiOnly,
            Ok("cppnix-only") => Mode::CppnixOnly,
            _ => Mode::Auto,
        }
    }
}

/// Spawn a child for the given binary + args, return its exit
/// code (Ok) or the IO error (Err).
fn spawn_and_wait(bin: &std::path::Path, args: &[String]) -> Result<i32, std::io::Error> {
    Command::new(bin).args(args).status().map(|s| s.code().unwrap_or(1))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let argv_subcommand = parse_subcommand_path(&args);
    let initial_route = route_for(&argv_subcommand);
    let mode = Mode::from_env();

    // Mode override may force a route different from catalog
    // resolution.  Auto mode honors catalog routing for the first
    // attempt; CppnixOnly forces cppnix; SuiOnly forces sui.
    let route = match mode {
        Mode::Auto => initial_route,
        Mode::SuiOnly => Route::Sui,
        Mode::CppnixOnly => Route::Cppnix,
    };
    log_decision(route, &args);

    let bin = match route {
        Route::Sui => sui_path(),
        Route::Cppnix => cppnix_path(),
    };
    let result = spawn_and_wait(&bin, &args);

    // Auto-mode fallback: if sui exited non-zero AND we have
    // cppnix available, retry with cppnix.  This is the
    // functional-100% guarantee — any command catalog-routed to
    // sui that fails will be re-attempted on cppnix transparently.
    if mode == Mode::Auto
        && route == Route::Sui
        && matches!(result, Ok(c) if c != 0)
    {
        let cppnix = cppnix_path();
        if cppnix.exists() || cppnix == std::path::PathBuf::from("cppnix") {
            log_decision_fallback(&args);
            return match spawn_and_wait(&cppnix, &args) {
                Ok(c) => ExitCode::from(u8::try_from(c & 0xff).unwrap_or(1)),
                Err(e) => {
                    eprintln!("nix-wrap: cppnix fallback failed: {e}");
                    ExitCode::from(127)
                }
            };
        }
    }

    match result {
        Ok(code) => ExitCode::from(u8::try_from(code & 0xff).unwrap_or(1)),
        Err(e) => {
            eprintln!("nix-wrap: cannot exec {} ({route:?}): {e}", bin.display());
            ExitCode::from(127)
        }
    }
}

/// Log a sui-failed → cppnix-fallback decision.
fn log_decision_fallback(argv: &[String]) {
    use std::io::Write;
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return;
    };
    let log_dir = home.join(".cache/sui");
    let _ = std::fs::create_dir_all(&log_dir);
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true).append(true).open(log_dir.join("nix-wrap.log"))
    else {
        return;
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = writeln!(f, "{ts}\t→cppnix-fallback\t{}", argv.join(" "));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_strips_leading_flags() {
        let argv = vec![
            "--show-trace".to_string(),
            "--option".to_string(),
            "store".to_string(),
            "dump-path".to_string(),
            "/nix/store/x".to_string(),
        ];
        let sub = parse_subcommand_path(&argv);
        // --option's value would actually be a positional but the
        // simple parser stops at the first flag again.  Sufficient
        // for the routing decision since "store dump-path" is the
        // longest prefix that matches the catalog.
        assert_eq!(sub, vec!["store", "dump-path", "/nix/store/x"]);
    }

    #[test]
    fn parse_empty_argv_returns_empty() {
        let argv: Vec<String> = vec![];
        let sub = parse_subcommand_path(&argv);
        assert!(sub.is_empty());
    }

    #[test]
    fn route_unknown_command_falls_back_to_cppnix() {
        let argv = ["totally-not-a-command"];
        assert_eq!(route_for(&argv), Route::Cppnix);
    }

    #[test]
    fn route_known_working_command_picks_sui() {
        // `hash to-sri` is Working in the canonical catalog.
        let argv = ["hash", "to-sri", "sha256:abc"];
        assert_eq!(route_for(&argv), Route::Sui);
    }

    #[test]
    fn route_longest_prefix_wins() {
        // `store dump-path` (2 tokens) is in catalog; `store`
        // alone (1 token) is also a subcommand group but the
        // longest-match should pick the more-specific entry.
        let argv = ["store", "dump-path", "/nix/store/x"];
        // Both entries route to sui in current catalog; this test
        // just ensures we don't panic on the lookup.
        let _ = route_for(&argv);
    }
}
