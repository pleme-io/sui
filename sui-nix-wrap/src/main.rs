//! `nix-wrap` — sui-only `nix` shim. **No cppnix fallback ever.**
//!
//! The operator installs this binary as `/run/current-system/sw/bin/nix`.
//! Every `nix <cmd> ...` invocation hits this wrapper, which:
//!
//! 1. Parses the subcommand path from `argv[1..]` (handles nested
//!    cases: `nix store dump-path`, `nix flake show`, etc).
//! 2. Looks up the matching entry in
//!    [`sui_spec::cli_coverage`]'s typed catalog.
//! 3. Routes to **sui** when the entry is `Working` or `SuiNative`.
//! 4. **For any other case** — `Partial` / `Stub` / `Missing`
//!    catalog entries, or commands absent from the catalog entirely
//!    — exits with a typed `coverage-gap` error message + nonzero
//!    status. **There is no cppnix fallback.** The gap surfaces
//!    immediately so the operator (and the compounding-directive
//!    "solve once" discipline) knows exactly what to close.
//! 5. Logs every routing decision to `~/.cache/sui/nix-wrap.log`
//!    so the operator can see which commands fired on sui and
//!    which raised coverage-gap errors.
//!
//! **Why no fallback:** sui replaces nix completely in Rust. Silent
//! cppnix retries would let the substrate degrade without anyone
//! noticing. Erroring loudly is the only way the gap-closure work
//! happens in measurable increments. Per the pleme-io directive:
//! "no fallback ever."
//!
//! Per pleme-io's NO SHELL law: every dispatch path is typed Rust.
//! No bash wrappers, no shell glue beyond the operator's
//! `alias nix=nix-wrap` (which is itself optional once the binary
//! lives on PATH).

use std::process::{Command, ExitCode};

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
#[derive(Debug, Clone, PartialEq, Eq)]
enum Route {
    /// Run on sui — command is `Working` or `SuiNative` per catalog.
    Sui,
    /// Coverage gap — command is `Stub`, `Partial`, `Missing`, or
    /// absent from the catalog. Errors out with a typed message;
    /// no fallback.
    Gap {
        /// The longest argv prefix that landed on the catalog (empty
        /// if no entry matched at all).
        matched_name: String,
        /// Catalog maturity if a name matched; "absent" otherwise.
        maturity: &'static str,
    },
}

impl Route {
    fn glyph(&self) -> &'static str {
        match self {
            Route::Sui => "→sui",
            Route::Gap { .. } => "✘gap",
        }
    }
}

/// Match a parsed command-path against the typed catalog.
///
/// Longest match first (so `nix store dump-path` hits the 2-token
/// catalog entry, not the 1-token `store`). Returns `Sui` for
/// Working/SuiNative entries **whose grade is a claim about the
/// platform we are running on**; `Gap` for everything else (including
/// commands absent from the catalog).
///
/// **The platform check is not a refinement, it is the point.** A row's
/// `maturity` is only ever a claim about its `platforms`, and routing on
/// the grade alone once sent NixOS operators into `system rebuild` —
/// graded `Working`, but whose activation arm execs nix-darwin's
/// `${system_path}/activate` and never NixOS's
/// `${toplevel}/bin/switch-to-configuration`. Reading the grade without
/// reading its scope is how a true claim becomes a false route.
fn route_for(argv_subcommand: &[&str]) -> Route {
    route_for_on(argv_subcommand, std::env::consts::OS)
}

/// [`route_for`], with the platform injected so the decision is testable
/// without cross-compiling — the seam that lets a linux CI run assert the
/// darwin behaviour and vice versa.
fn route_for_on(argv_subcommand: &[&str], os: &str) -> Route {
    let Ok(catalog) = sui_spec::cli_coverage::load_canonical() else {
        return Route::Gap {
            matched_name: argv_subcommand.join(" "),
            maturity: "catalog-load-failed",
        };
    };
    for take in (1..=argv_subcommand.len()).rev() {
        let candidate = argv_subcommand[..take].join(" ");
        for entry in &catalog {
            if entry.name == candidate {
                use sui_spec::cli_coverage::SuiCommandMaturity::*;
                if !entry.claims_platform(os) {
                    return Route::Gap {
                        matched_name: candidate,
                        maturity: "not-claimed-on-this-platform",
                    };
                }
                return match entry.maturity {
                    Working | SuiNative => Route::Sui,
                    Partial => Route::Gap {
                        matched_name: candidate,
                        maturity: "Partial",
                    },
                    Stub => Route::Gap {
                        matched_name: candidate,
                        maturity: "Stub",
                    },
                    Missing => Route::Gap {
                        matched_name: candidate,
                        maturity: "Missing",
                    },
                };
            }
        }
    }
    Route::Gap {
        matched_name: argv_subcommand.join(" "),
        maturity: "absent",
    }
}

/// Append one routing decision to the operator-facing log.
///
/// Best-effort: any I/O error is silently dropped (the wrapper
/// must not fail an invocation because the log directory wasn't
/// writable).
fn log_decision(route: &Route, argv: &[String]) {
    use std::io::Write;
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return;
    };
    let log_dir = home.join(".cache/sui");
    if std::fs::create_dir_all(&log_dir).is_err() {
        return;
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("nix-wrap.log"))
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
/// token. e.g. `--show-trace store dump-path /nix/store/...` →
/// `["store", "dump-path", "/nix/store/..."]`.
fn parse_subcommand_path(args: &[String]) -> Vec<&str> {
    args.iter()
        .skip_while(|a| a.starts_with('-'))
        .take_while(|a| !a.starts_with('-'))
        .map(String::as_str)
        .collect()
}

/// Spawn the configured sui binary with the operator's argv and
/// wait for it. Returns the child's exit code or the IO error.
fn spawn_sui(args: &[String]) -> Result<i32, std::io::Error> {
    let bin = sui_path();
    Command::new(&bin)
        .args(args)
        .status()
        .map(|s| s.code().unwrap_or(1))
}

/// Print the typed coverage-gap message + exit nonzero. The message
/// format is stable; operators and tooling parse `coverage-gap:` as
/// the prefix.
fn print_gap_message(matched_name: &str, maturity: &str, argv: &[String]) {
    eprintln!(
        "nix-wrap: coverage-gap: '{cmd}' is {status} in sui's CLI catalog",
        cmd = if matched_name.is_empty() { "(unknown)" } else { matched_name },
        status = maturity,
    );
    eprintln!("  invocation : nix {}", argv.join(" "));
    eprintln!("  catalog    : sui-spec/specs/cli_coverage.lisp");
    eprintln!(
        "  closure    : implement the command natively in sui; bump catalog \
         entry to Working; add the matching parity probe."
    );
    eprintln!(
        "  rationale  : sui replaces nix completely in Rust. No cppnix \
         fallback — every gap is a measurable closure target."
    );
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let argv_subcommand = parse_subcommand_path(&args);
    let route = route_for(&argv_subcommand);
    log_decision(&route, &args);

    match &route {
        Route::Sui => match spawn_sui(&args) {
            Ok(code) => ExitCode::from(u8::try_from(code & 0xff).unwrap_or(1)),
            Err(e) => {
                eprintln!("nix-wrap: cannot exec sui ({}): {e}", sui_path().display());
                ExitCode::from(127)
            }
        },
        Route::Gap { matched_name, maturity } => {
            print_gap_message(matched_name, maturity, &args);
            // Stable exit code so wrapper scripts / CI gates can
            // detect coverage-gap exits specifically.
            ExitCode::from(78)
        }
    }
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
        assert_eq!(sub, vec!["store", "dump-path", "/nix/store/x"]);
    }

    #[test]
    fn parse_empty_argv_returns_empty() {
        let argv: Vec<String> = vec![];
        let sub = parse_subcommand_path(&argv);
        assert!(sub.is_empty());
    }

    #[test]
    fn route_unknown_command_is_typed_gap_not_fallback() {
        let argv = ["totally-not-a-command"];
        match route_for(&argv) {
            Route::Gap { matched_name, maturity } => {
                assert_eq!(matched_name, "totally-not-a-command");
                assert_eq!(maturity, "absent");
            }
            other => panic!("expected coverage-gap, got {other:?}"),
        }
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
        // alone is the prefix. The longest match should pick the
        // more-specific entry.
        //
        // This test used to read `let _ = route_for(&argv);` with the
        // comment "just don't panic" — it asserted NOTHING and would have
        // passed against any routing decision whatsoever, including the
        // wrong one. Truthing the instruments (theory/BALIZA.md phase 0a)
        // means a test that cannot fail is worse than no test, because it
        // reports coverage it does not have.
        let argv = ["store", "dump-path", "/nix/store/x"];
        assert!(
            matches!(route_for_on(&argv, "linux"), Route::Sui),
            "`store dump-path` is Working and platform-independent, so it routes to sui"
        );
    }

    /// The regression this whole field exists for.
    ///
    /// `system rebuild` is graded `Working` and that grade is TRUE — on
    /// darwin. Routing it on linux sent the operator into an activation
    /// arm that execs nix-darwin's `${system_path}/activate` and never
    /// NixOS's `${toplevel}/bin/switch-to-configuration`.
    ///
    /// RED-RUN RECEIPT (2026-08-07): deleting the `:platforms ("macos")`
    /// line from the `system rebuild` row in `cli_coverage.lisp` turns
    /// this test red with `expected Gap, got Sui` — the assertion is not
    /// vacuous, and it is anchored to the catalog row rather than to a
    /// stub.
    #[test]
    fn system_rebuild_does_not_route_to_sui_on_linux() {
        let argv = ["system", "rebuild"];
        match route_for_on(&argv, "linux") {
            Route::Gap { maturity, .. } => {
                assert_eq!(maturity, "not-claimed-on-this-platform");
            }
            Route::Sui => panic!(
                "`system rebuild` routed to sui on linux — sui's activation arm \
                 cannot activate a NixOS system (see theory/BALIZA.md §III)"
            ),
        }
    }

    /// The other half, so the fix is a QUALIFICATION and not a round-down:
    /// darwin keeps the capability it genuinely has. A round-down discards
    /// true signal exactly as a round-up ships false.
    #[test]
    fn system_rebuild_still_routes_to_sui_on_darwin() {
        let argv = ["system", "rebuild"];
        assert!(
            matches!(route_for_on(&argv, "macos"), Route::Sui),
            "darwin activation is the path sui actually implements; \
             qualifying the row must not cost darwin its capability"
        );
    }

    /// `system rollback` shares the broken arm — both rollback paths end
    /// in `activate_system(.., Switch)` — so it must be qualified too. A
    /// rollback lifeline that cannot restore boot state is a slower path
    /// to the same brick.
    #[test]
    fn system_rollback_does_not_route_to_sui_on_linux() {
        assert!(
            matches!(route_for_on(&["system", "rollback"], "linux"), Route::Gap { .. }),
            "`system rollback` does not restore boot state on NixOS"
        );
    }

    /// An unannotated row means "platform-independent", which is the
    /// overwhelming majority of the catalog. If this ever fails, the
    /// default flipped and every un-annotated row silently stopped
    /// routing — a catastrophic round-down hiding as a safety fix.
    #[test]
    fn unannotated_rows_claim_every_platform() {
        for os in ["linux", "macos"] {
            assert!(
                matches!(route_for_on(&["hash", "file"], os), Route::Sui),
                "`hash file` carries no :platforms and must route everywhere (os={os})"
            );
        }
    }

    #[test]
    fn glyph_marks_gap_distinctly_from_sui() {
        assert_eq!(Route::Sui.glyph(), "→sui");
        let gap = Route::Gap {
            matched_name: "x".into(),
            maturity: "absent",
        };
        assert_eq!(gap.glyph(), "✘gap");
    }
}
