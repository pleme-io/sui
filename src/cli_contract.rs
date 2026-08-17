//! The CLI flag partition — one table saying, for every declared argument,
//! whether sui actually honours it.
//!
//! # Why this exists
//!
//! An audit on 2026-08-17 found ~25 places where clap parsed a flag and the
//! handler destructured it to `_` (or swallowed it under `{ .. }`). The flag
//! was accepted, silently ignored, and the command exited **0 with a confident
//! wrong answer**:
//!
//! * `cache push --cache-url https://cache.example` wrote to the LOCAL disk and
//!   printed `pushed <path>`. Every consumer of the shared cache then 404s.
//! * `profile upgrade` printed `upgraded: <name>` for elements it had not
//!   upgraded, *and* overwrote each element's locked `url` with its unlocked
//!   `originalUrl` — data loss, not merely a wrong answer.
//! * `profile wipe-history --older-than 30d` deleted every generation but the
//!   newest. An ignored argument that NARROWS a destructive operation is the
//!   worst shape there is: the default is the wider blast radius.
//!
//! # Why a table and not the type system
//!
//! Rust cannot make "declared but unread" a compile error. `unused_variables`
//! never fires on `field: _` or on a `{ .. }` rest-pattern — which is exactly
//! why 25 of these survived review, individually, for months. There is no
//! lint, no `#[must_use]`, no borrow-check obligation that a struct field must
//! be read.
//!
//! So the honest mechanism is **reflection plus a total partition**:
//!
//! 1. [`CONTRACT`] is the single definer. Every `(subcommand path, arg id)` pair
//!    appears exactly once, carrying a verdict.
//! 2. [`enforce`] runs ONCE in `main`, before dispatch. A [`Honour::Refused`]
//!    argument that the operator actually typed exits **2** with the typed
//!    reason instead of running the command.
//! 3. [`tests::every_declared_arg_is_classified`] walks the BUILT
//!    `Cli::command()` and asserts set-equality with [`CONTRACT`] in **both**
//!    directions. A new `#[arg]` that nobody classified fails the build on the
//!    commit that adds it; a `CONTRACT` row naming an arg that no longer exists
//!    fails too.
//!
//! **TIER — stated, not rounded up.** This is a *test-caught* gate, not a
//! type-level guarantee. The illegal state (an unclassified arg) is still
//! representable; it is merely caught by `cargo test`. It is the closest honest
//! thing Rust offers here, and it is strictly stronger than the review process
//! that let 25 instances through. It does NOT prove a handler reads a field it
//! declared `Honoured` — only that a human has stated a verdict for every arg
//! and that the verdict is enforced when it says `Refused`.
//!
//! # The two verdicts, and what `Honoured` means
//!
//! `Honoured` means **the behaviour the operator asked for is the behaviour
//! they get**. That includes *vacuous* satisfaction — a flag that asks sui to
//! do less of something sui never did, or to select what is already the
//! default. Those rows carry the vacuity in a comment, because "the handler
//! never reads it" and "the handler cannot do otherwise" look identical in a
//! diff and are not the same fact.
//!
//! `Refused` means sui does not deliver what the flag says. Refusing is the
//! honest answer for two shapes:
//!
//! * the flag would change the result and does not (a wrong answer), and
//! * the flag names a command sui has not implemented at all (the command
//!   errors anyway; refusing names the *flag* rather than the command, which is
//!   the more specific truth).
//!
//! # `value_source` is load-bearing
//!
//! [`enforce`] refuses only on [`ValueSource::CommandLine`]. A clap
//! `default_value` also populates the match, and refusing on those would make
//! every defaulted argument trip the gate — the command would become
//! unrunnable. This is the one detail that decides whether the partition is
//! usable at all.

use clap::ArgMatches;
use clap::parser::ValueSource;

use sui::CliError;

/// Whether sui delivers what a declared argument promises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Honour {
    /// The operator gets the behaviour the flag names — including vacuously,
    /// where sui's only behaviour already IS what the flag asks for.
    Honoured,
    /// sui does not deliver what the flag names. Supplying it on the command
    /// line exits 2 rather than producing a confident wrong answer.
    Refused {
        /// The date this refusal landed, so a reader can date the claim.
        since: &'static str,
        /// What the operator asked for, what sui actually does, and — where one
        /// exists — the destination that would let the flag be honoured.
        reason: &'static str,
    },
}

/// Shorthand for the common row. Spelled out for refusals so the reason sits
/// at the call site rather than behind an alias.
const H: Honour = Honour::Honoured;

/// The date every refusal in this pass landed.
const NOW: &str = "2026-08-17";

/// The single definer: every `(subcommand path, arg id)` sui declares, with its
/// verdict.
///
/// * The path is the chain of subcommand **names** (kebab-case as clap renders
///   them), so `&[]` is the root's global arguments and `&["cache", "push"]` is
///   `sui cache push`.
/// * The arg id is clap's id — the Rust field name, `snake_case`, NOT the
///   `--kebab-case` spelling.
///
/// Ordering follows the declaration order in `main.rs` so the two read side by
/// side.
pub const CONTRACT: &[(&[&str], &str, Honour)] = &[
    // ── root: global arguments ──────────────────────────────────────────
    //
    // `--vm` selects the bytecode VM. It used to be VACUOUSLY honoured —
    // `main` branched on `no_vm` alone, so the VM was the default and passing
    // `--vm` took the same code path as passing nothing.
    //
    // Since the 2026-08-17 flip the tree-walker is the default and `main`
    // branches on `vm`, so this flag now genuinely selects an engine.
    // `--no-vm` is the one that became a no-op: it names the default and is
    // kept so existing scripts keep working. Both remain Honoured, but for
    // opposite reasons than before — worth stating, because a flag whose
    // meaning inverted while its classification stayed put is exactly the kind
    // of drift this table exists to make visible.
    (&[], "vm", H),
    (&[], "no_vm", H),
    (&[], "show_trace", H),
    (
        &[],
        "print_build_logs",
        Honour::Refused {
            since: NOW,
            reason: "-L/--print-build-logs asks sui to BREAK the silence around a build; sui \
                     reads it nowhere, so the operator gets exactly the silence the flag exists \
                     to end and concludes the build printed nothing",
        },
    ),
    // The eleven flags below were absorbed so `alias nix=sui` would PARSE every
    // `nix …` invocation. Absorb-and-ignore is precisely the defect this module
    // exists to remove: parsing a flag is not honouring it. The destination is
    // cppnix fallthrough (`exec_cppnix_passthrough`, already in `main.rs`) for
    // any nix flag sui cannot honour; until that lands, refusing is the honest
    // interim. Measured 2026-08-17 across 221,311 fleet files: no `alias
    // nix=sui` exists anywhere, so nothing calls sui with these today.
    (
        &[],
        "extra_experimental_features",
        Honour::Refused {
            since: NOW,
            reason: "nix parse-compat only: sui gates no behaviour behind an experimental \
                     feature, so enabling one cannot mean anything; destination is cppnix \
                     fallthrough",
        },
    ),
    (
        &[],
        "no_write_lock_file",
        Honour::Refused {
            since: NOW,
            reason: "the operator directs sui NOT to write flake.lock; sui reads the flag \
                     nowhere and its lock path is free to write anyway — a forbidden mutation, \
                     not just a wrong answer",
        },
    ),
    (
        &[],
        "accept_flake_config",
        Honour::Refused {
            since: NOW,
            reason: "grants permission to apply a flake's own nixConfig; sui applies no flake \
                     nixConfig at all, so the grant is unimplemented rather than merely unused",
        },
    ),
    // `--impure` asks for impure evaluation, and sui evaluates impurely
    // ALREADY — measured, not assumed: without the flag, `builtins.getEnv
    // "HOME"` returns the real environment variable and `builtins.currentSystem`
    // resolves to the host double. sui implements no purity restriction at all,
    // so the behaviour requested is the behaviour delivered.
    //
    // This row was briefly Refused, on the reasoning that "sui reads it
    // nowhere, so an expression that needs impurity evaluates under whatever
    // purity sui defaults to — a different answer, silently". That reasoning is
    // wrong in its premise: sui's default IS impure, so there is no different
    // answer to be had.
    //
    // ★ IT ALSO BROKE THE BYTE-PARITY GATE. sui's own parity corpus drives
    // `sui eval --impure` (sui-spec/src/cli.rs), so refusing it made 70 of 77
    // rows `SuiError` and `parity.yml` went red for six consecutive runs. The
    // fleet-caller sweep that preceded the refusal checked other REPOSITORIES
    // and found no `alias nix=sui` — it did not check sui's own test corpus,
    // which is the one caller that was guaranteed to exist.
    //
    // Same shape as `copy --no-check-sigs`, kept Honoured for the same reason:
    // a flag asking for LESS restriction, on an implementation that applies no
    // restriction, is honoured rather than vacuous. The day sui grows a pure
    // mode, this row inverts and `--pure-eval` (which does not exist yet) is
    // the one to add.
    (&[], "impure", H),
    (
        &[],
        "option",
        Honour::Refused {
            since: NOW,
            reason: "`--option <k> <v>` sets an arbitrary nix setting; sui applies none of \
                     them, so any setting the operator relied on is silently absent",
        },
    ),
    (
        &[],
        "log_format",
        Honour::Refused {
            since: NOW,
            reason: "selects a log format a downstream parser expects; sui emits its own \
                     format regardless, so a machine consumer parses the wrong shape",
        },
    ),
    (
        &[],
        "max_jobs",
        Honour::Refused {
            since: NOW,
            reason: "bounds build parallelism; sui reads it nowhere, so a caller throttling a \
                     shared machine gets no throttle",
        },
    ),
    (
        &[],
        "cores",
        Honour::Refused {
            since: NOW,
            reason: "bounds per-build core count; sui reads it nowhere, so a caller throttling \
                     a shared machine gets no throttle",
        },
    ),
    (
        &[],
        "keep_going",
        Honour::Refused {
            since: NOW,
            reason: "changes failure semantics (finish independent derivations after one \
                     fails); sui reads it nowhere, so the operator cannot tell a stop-at-first \
                     run from a keep-going run",
        },
    ),
    (
        &[],
        "verbose",
        Honour::Refused {
            since: NOW,
            reason: "asks for more diagnostic output; sui reads it nowhere, so the operator \
                     reads a quiet run as a clean one",
        },
    ),
    (
        &[],
        "quiet",
        Honour::Refused {
            since: NOW,
            reason: "asks for less output; sui reads it nowhere, so a caller capturing stdout \
                     gets noise it explicitly suppressed",
        },
    ),
    // ── serve ───────────────────────────────────────────────────────────
    (&["serve"], "listen", H),
    (&["serve"], "grpc_listen", H),
    // ── eval ────────────────────────────────────────────────────────────
    (&["eval"], "expression", H),
    (&["eval"], "json", H),
    (&["eval"], "raw", H),
    (&["eval"], "expr_flag", H),
    (&["eval"], "max_force_depth", H),
    (&["eval"], "no_eval_cache", H),
    (
        &["eval"],
        "apply",
        Honour::Refused {
            since: NOW,
            reason: "`--apply <f>` names a function to apply to the evaluated value; sui \
                     discards it and prints the UNAPPLIED value, which is a different value \
                     wearing the right exit code",
        },
    ),
    (
        &["eval"],
        "file_flag",
        Honour::Refused {
            since: NOW,
            reason: "`-f/--file <path>` selects the file to evaluate; sui discards it and \
                     evaluates the positional expression instead, so the operator reads a \
                     result from the wrong source",
        },
    ),
    // ── build ───────────────────────────────────────────────────────────
    (&["build"], "installable", H),
    (
        &["build"],
        "no_link",
        Honour::Refused {
            since: NOW,
            reason: "suppresses the `result` symlink; sui reads it nowhere, so a caller that \
                     asked for no filesystem side-effect may get one",
        },
    ),
    (
        &["build"],
        "print_out_paths",
        Honour::Refused {
            since: NOW,
            reason: "makes stdout the out-path list a script then consumes; sui reads it \
                     nowhere, so the script parses whatever sui prints instead",
        },
    ),
    (
        &["build"],
        "json",
        Honour::Refused {
            since: NOW,
            reason: "asks for machine-readable output; sui prints human text regardless, so a \
                     JSON consumer fails to parse — or worse, half-parses",
        },
    ),
    // `--dry-run` was wired on both arms before this pass; it is read.
    (&["build"], "dry_run", H),
    (
        &["build"],
        "out_link",
        Honour::Refused {
            since: NOW,
            reason: "`-o/--out-link <p>` names where the result symlink goes; sui reads it \
                     nowhere, so the link lands at the default path and the caller's next step \
                     reads a stale or absent `p`",
        },
    ),
    (
        &["build"],
        "rebuild",
        Honour::Refused {
            since: NOW,
            reason: "forces a rebuild to check reproducibility; sui reads it nowhere, so a \
                     cache hit is reported as a successful rebuild — the exact answer the flag \
                     exists to distrust",
        },
    ),
    // ── daemon ──────────────────────────────────────────────────────────
    (&["daemon"], "socket", H),
    // ── develop ─────────────────────────────────────────────────────────
    (&["develop"], "flake_ref", H),
    (&["develop"], "attr", H),
    (&["develop"], "command", H),
    // ── run ─────────────────────────────────────────────────────────────
    (&["run"], "installable", H),
    (&["run"], "args", H),
    // ── search ──────────────────────────────────────────────────────────
    (&["search"], "flake_ref", H),
    (&["search"], "query", H),
    // ── repl (the whole command is unimplemented) ───────────────────────
    (
        &["repl"],
        "flake_ref",
        Honour::Refused {
            since: NOW,
            reason: "`sui repl` is unimplemented; the flake ref is parsed and discarded",
        },
    ),
    (
        &["repl"],
        "file",
        Honour::Refused {
            since: NOW,
            reason: "`sui repl` is unimplemented; the file is parsed and discarded",
        },
    ),
    // ── copy ────────────────────────────────────────────────────────────
    (&["copy"], "to", H),
    (&["copy"], "from", H),
    (&["copy"], "paths", H),
    // VACUOUSLY HONOURED, and deliberately so. `--no-check-sigs` asks copy to
    // do LESS verification; `cmd_copy` verifies no signature at any point, so
    // the behaviour the operator asked for is the behaviour they get. Refusing
    // it would be a pure regression: three live fleet call sites pass it
    // (`actions/super-cache-save/run.tlisp`, `actions/flake-input-preseed/run.tlisp`,
    // `tatara/tatara-build-remote/src/transports.rs`), all as `nix copy …`, all
    // reaching sui the moment `alias nix=sui` is set. Re-examine this row if
    // copy ever gains signature verification — the vacuity ends the same day.
    (&["copy"], "no_check_sigs", H),
    // ── path-info ───────────────────────────────────────────────────────
    (&["path-info"], "paths", H),
    (&["path-info"], "json", H),
    (
        &["path-info"],
        "closure_size",
        Honour::Refused {
            since: NOW,
            reason: "asks for the closure SIZE column; sui reads it nowhere and prints path \
                     info without it, so a size the operator asked for is simply absent from \
                     an otherwise-successful report",
        },
    ),
    // ── collect-garbage (both arms wired before this pass) ──────────────
    (&["collect-garbage"], "delete_old", H),
    (&["collect-garbage"], "delete_older_than", H),
    // ── show-config ─────────────────────────────────────────────────────
    (&["show-config"], "json", H),
    // ── why / path-from-hash-part / edit / log / diff-closures ──────────
    (&["why"], "path", H),
    (&["why"], "dependency", H),
    (&["path-from-hash-part"], "hash_part", H),
    (&["edit"], "installable", H),
    (&["log"], "installable", H),
    (&["store-diff-closures"], "before", H),
    (&["store-diff-closures"], "after", H),
    // ── upgrade-nix (the whole command is unimplemented) ────────────────
    (
        &["upgrade-nix"],
        "nix_store_paths_url",
        Honour::Refused {
            since: NOW,
            reason: "`sui upgrade-nix` is unimplemented; the URL is parsed and discarded",
        },
    ),
    // ── fmt ─────────────────────────────────────────────────────────────
    (&["fmt"], "files", H),
    (&["fmt"], "check", H),
    // ── agent ───────────────────────────────────────────────────────────
    (&["agent"], "nats_url", H),
    (&["agent"], "stream", H),
    (&["agent"], "consumer", H),
    (&["agent"], "cache_url", H),
    (&["agent"], "cache_name", H),
    (&["agent"], "strategy", H),
    (&["agent"], "signing_key", H),
    // ── cache-warm ──────────────────────────────────────────────────────
    (&["cache-warm"], "flake_ref", H),
    (&["cache-warm"], "attrs", H),
    // ── parity / build-parity / parity-bisect / perf-seal ───────────────
    (&["parity"], "nix", H),
    (&["parity"], "json", H),
    (&["parity"], "track_nixpkgs", H),
    (&["build-parity"], "nix", H),
    (&["parity-bisect"], "expr", H),
    (&["parity-bisect"], "nix", H),
    (&["perf-seal"], "json", H),
    (&["perf-seal"], "write_baseline", H),
    // ── print-dev-env (the whole command is unimplemented) ──────────────
    (
        &["print-dev-env"],
        "flake_ref",
        Honour::Refused {
            since: NOW,
            reason: "`sui print-dev-env` is unimplemented; the flake ref reaches only the \
                     error message",
        },
    ),
    (
        &["print-dev-env"],
        "json",
        Honour::Refused {
            since: NOW,
            reason: "`sui print-dev-env` is unimplemented; the flag is parsed and discarded",
        },
    ),
    // ── bundle (the whole command is unimplemented) ─────────────────────
    (
        &["bundle"],
        "installable",
        Honour::Refused {
            since: NOW,
            reason: "`sui bundle` is unimplemented; the installable reaches only the error \
                     message",
        },
    ),
    (
        &["bundle"],
        "bundler",
        Honour::Refused {
            since: NOW,
            reason: "`sui bundle` is unimplemented; the bundler reaches only the error message",
        },
    ),
    (
        &["bundle"],
        "out_link",
        Honour::Refused {
            since: NOW,
            reason: "`sui bundle` is unimplemented; the out-link is parsed and discarded",
        },
    ),
    // ── rebuild-shadow ──────────────────────────────────────────────────
    (&["rebuild-shadow"], "flakes", H),
    (&["rebuild-shadow"], "nix", H),
    (&["rebuild-shadow"], "flakes_root", H),
    (&["rebuild-shadow"], "corpus", H),
    (&["rebuild-shadow"], "tag", H),
    (&["rebuild-shadow"], "skip_tag", H),
    (&["rebuild-shadow"], "timeout_secs", H),
    (&["rebuild-shadow"], "report", H),
    (&["rebuild-shadow"], "no_report", H),
    (&["rebuild-shadow"], "verbose_probes", H),
    // ── store … ─────────────────────────────────────────────────────────
    (&["store", "path-info"], "path", H),
    (&["store", "path-info"], "json", H),
    (&["store", "paths"], "limit", H),
    (&["store", "gc"], "max_age_days", H),
    (&["store", "gc"], "print_roots", H),
    (&["store", "gc"], "dry_run", H),
    (&["store", "optimise"], "dry_run", H),
    (&["store", "delete"], "paths", H),
    // Wired before this pass: `store delete` REFUSES without it.
    (&["store", "delete"], "ignore_liveness", H),
    (&["store", "ls"], "path", H),
    (&["store", "ls"], "recursive", H),
    (&["store", "ls"], "long", H),
    (&["store", "ls"], "json", H),
    (&["store", "cat"], "path", H),
    (&["store", "dump-path"], "path", H),
    (&["store", "make-content-addressed"], "paths", H),
    (&["store", "add-path"], "path", H),
    (&["store", "add-path"], "name", H),
    (&["store", "add-file"], "path", H),
    (&["store", "add-file"], "name", H),
    (&["store", "prefetch-file"], "url", H),
    (&["store", "prefetch-file"], "name", H),
    (&["store", "prefetch-file"], "hash", H),
    (&["store", "prefetch-file"], "hash_type", H),
    (&["store", "prefetch-file"], "unpack", H),
    (&["store", "sign"], "paths", H),
    (&["store", "sign"], "key_file", H),
    (&["store", "repair"], "paths", H),
    (&["store", "inventory"], "profile", H),
    (&["store", "inventory"], "json", H),
    (&["store", "closure"], "path", H),
    (&["store", "closure"], "json", H),
    (&["store", "materialize"], "slice", H),
    (&["store", "materialize"], "dest", H),
    (&["store", "materialize"], "json", H),
    (&["store", "transform"], "source", H),
    (&["store", "transform"], "transform", H),
    (&["store", "transform"], "dest", H),
    (&["store", "transform"], "json", H),
    (&["store", "diff"], "a", H),
    (&["store", "diff"], "b", H),
    (&["store", "diff"], "json", H),
    (&["store", "graft"], "root", H),
    (&["store", "graft"], "from", H),
    (&["store", "graft"], "to", H),
    (&["store", "graft"], "dest", H),
    (&["store", "graft"], "json", H),
    (&["store", "audit-secrets"], "source", H),
    (&["store", "audit-secrets"], "json", H),
    (&["store", "fingerprint"], "path", H),
    (&["store", "fingerprint"], "json", H),
    (&["store", "find"], "profile", H),
    (&["store", "find"], "name", H),
    (&["store", "find"], "min_size", H),
    (&["store", "find"], "max_size", H),
    (&["store", "find"], "contents", H),
    (&["store", "find"], "json", H),
    (&["store", "stats"], "profile", H),
    (&["store", "stats"], "json", H),
    (&["store", "analyze"], "profile", H),
    (&["store", "analyze"], "no_duplicates", H),
    (&["store", "analyze"], "high_fanout_threshold", H),
    (&["store", "analyze"], "json", H),
    (&["store", "upgrade-paths"], "profile", H),
    (&["store", "upgrade-paths"], "json", H),
    (&["store", "recipe"], "name", H),
    (&["store", "recipe"], "dest_base", H),
    (&["store", "recipe"], "json", H),
    (&["store", "fingerprint-many"], "profile", H),
    (&["store", "fingerprint-many"], "out", H),
    (&["store", "compare-manifests"], "a", H),
    (&["store", "compare-manifests"], "b", H),
    (&["store", "dedupe-plan"], "profile", H),
    (&["store", "dedupe-plan"], "json", H),
    (&["store", "entropy"], "path", H),
    (&["store", "entropy"], "json", H),
    (&["store", "ascii-graph"], "path", H),
    (&["store", "ascii-graph"], "max_depth", H),
    (&["store", "sbom"], "path", H),
    (&["store", "sbom"], "out", H),
    (&["store", "sign-manifest"], "manifest", H),
    (&["store", "sign-manifest"], "key_file", H),
    (&["store", "verify-manifest"], "manifest", H),
    (&["store", "verify-manifest"], "pubkey", H),
    (&["store", "verify-manifest"], "sig", H),
    (&["store", "license-scan"], "path", H),
    (&["store", "license-scan"], "json", H),
    (&["store", "cve-scan"], "path", H),
    (&["store", "cve-scan"], "pattern", H),
    (&["store", "cve-scan"], "json", H),
    // ── flake … ─────────────────────────────────────────────────────────
    (&["flake", "show"], "flake_ref", H),
    (&["flake", "show"], "json", H),
    (&["flake", "update"], "input", H),
    (&["flake", "check"], "flake_ref", H),
    (
        &["flake", "check"],
        "no_build",
        Honour::Refused {
            since: NOW,
            reason: "asks `flake check` to evaluate WITHOUT building; sui reads it nowhere, so \
                     a caller that meant to run a cheap eval-only gate may realize derivations \
                     it never asked for",
        },
    ),
    (&["flake", "metadata"], "flake_ref", H),
    (&["flake", "metadata"], "json", H),
    (&["flake", "init"], "template", H),
    (&["flake", "new"], "dest", H),
    (&["flake", "new"], "template", H),
    (&["flake", "archive"], "flake_ref", H),
    (&["flake", "archive"], "json", H),
    (&["flake", "clone"], "flake_ref", H),
    (&["flake", "clone"], "dest", H),
    (&["flake", "prefetch"], "flake_ref", H),
    (&["flake", "prefetch"], "json", H),
    // ── system … ────────────────────────────────────────────────────────
    (&["system", "rebuild"], "action", H),
    (&["system", "rebuild"], "flake", H),
    (&["system", "rebuild"], "dry_run", H),
    (&["system", "converge"], "flake", H),
    (&["system", "converge"], "watch", H),
    (&["system", "converge"], "interval_secs", H),
    (&["system", "converge"], "action", H),
    (&["system", "converge"], "shadow", H),
    // ── fleet … ─────────────────────────────────────────────────────────
    (&["fleet", "deploy"], "target", H),
    // ── cache … ─────────────────────────────────────────────────────────
    (&["cache", "serve"], "listen", H),
    (&["cache", "serve"], "store_path", H),
    (&["cache", "serve"], "priority", H),
    (&["cache", "serve"], "backend_config", H),
    (&["cache", "serve"], "supercache_config", H),
    (&["cache", "serve"], "signing_key", H),
    (&["cache", "push"], "paths", H),
    // PRIORITY 1. This is a PUBLISH path, which is why it is the first row that
    // mattered. `--cache-url` was destructured to `_`; the handler built a
    // `LocalStorage` over `--store-path` and printed `pushed <path>` per path.
    // The operator reads a success line and believes the artifact is on the
    // shared cache; it is on local disk, and every consumer 404s. The failure is
    // silent on the producing side and only ever surfaces on the consuming one.
    (
        &["cache", "push"],
        "cache_url",
        Honour::Refused {
            since: NOW,
            reason: "`cache push` writes to the LOCAL --store-path only; --cache-url was \
                     discarded while the command still printed `pushed <path>`, so an artifact \
                     the operator believes is on the shared cache is not — and only its \
                     consumers find out, as 404s. Push locally then serve, or use \
                     `sui cache serve` as the origin; remote push needs an HTTP backend that \
                     does not exist yet",
        },
    ),
    (&["cache", "push"], "store_path", H),
    (&["cache", "push"], "signing_key", H),
    (&["cache", "push"], "recursive", H),
    (&["cache", "gc"], "store_path", H),
    (&["cache", "gc"], "keep", H),
    (&["cache", "info"], "store_path", H),
    (&["cache", "watch"], "store_path", H),
    (&["cache", "watch"], "signing_key", H),
    (&["cache", "watch"], "interval_secs", H),
    (&["cache", "watch"], "once", H),
    (&["cache", "watch"], "initial_reconcile", H),
    (&["cache", "watch"], "max_per_pass", H),
    (&["cache", "resign"], "store_path", H),
    (&["cache", "resign"], "signing_key", H),
    (&["cache", "wipe"], "backend_config", H),
    (&["cache", "wipe"], "store_path", H),
    // ── profile … ───────────────────────────────────────────────────────
    //
    // PRIORITY 6. `--profile` was discarded by all eight subcommands, so every
    // one of them silently operated on the hardcoded default profile. It is
    // IMPLEMENTED rather than refused: refusing would punish the operator who
    // was explicit about which profile they meant, while continuing to serve
    // the one who said nothing and hit the default — inverting the safety
    // incentive on four MUTATING commands (install / remove / upgrade /
    // wipe-history). See `ProfilePath` in `main.rs`.
    (&["profile", "list"], "profile", H),
    (&["profile", "list"], "json", H),
    (&["profile", "install"], "packages", H),
    (&["profile", "install"], "profile", H),
    (
        &["profile", "install"],
        "priority",
        Honour::Refused {
            since: NOW,
            reason: "sets the collision priority a later install is resolved against; sui \
                     writes no priority into the manifest, so two packages that collide \
                     resolve by an order the operator did not choose",
        },
    ),
    (&["profile", "remove"], "packages", H),
    (&["profile", "remove"], "profile", H),
    (&["profile", "upgrade"], "packages", H),
    (&["profile", "upgrade"], "profile", H),
    (&["profile", "rollback"], "profile", H),
    (&["profile", "history"], "profile", H),
    (&["profile", "wipe-history"], "profile", H),
    // PRIORITY 3. An ignored argument that NARROWS a destructive operation must
    // be a refusal and never a default-to-wider. `--older-than 30d` was
    // discarded while the command deleted EVERY generation but the newest.
    (
        &["profile", "wipe-history"],
        "older_than",
        Honour::Refused {
            since: NOW,
            reason: "`--older-than` NARROWS which generations are deleted; it was discarded \
                     while the command removed every generation but the newest. Ignoring an \
                     argument that narrows a destructive operation defaults to the WIDER blast \
                     radius, so the safe spelling was the dangerous one",
        },
    ),
    (&["profile", "diff"], "profile", H),
    // ── derivation … ────────────────────────────────────────────────────
    (&["derivation", "show"], "paths", H),
    // VACUOUSLY HONOURED: `derivation show` emits JSON unconditionally (as
    // cppnix does), so `--json` selects the only output shape there is.
    (&["derivation", "show"], "json", H),
    (&["derivation", "add"], "path", H),
    (&["derivation", "graph"], "path", H),
    (&["derivation", "graph"], "max_depth", H),
    (&["derivation", "graph"], "json", H),
    // ── hash … ──────────────────────────────────────────────────────────
    (&["hash", "file"], "path", H),
    (&["hash", "file"], "type", H),
    (&["hash", "file"], "base", H),
    (&["hash", "path"], "path", H),
    (&["hash", "path"], "type", H),
    (&["hash", "path"], "base", H),
    // `hash to-baseN --type` names the algorithm to interpret a bare (unprefixed)
    // hash under. sui infers the algorithm from length instead and discards the
    // flag, so a caller disambiguating an ambiguous input gets the inference
    // anyway — which is the whole reason to pass it.
    (&["hash", "to-base16"], "hash", H),
    (
        &["hash", "to-base16"],
        "type",
        Honour::Refused {
            since: NOW,
            reason: "`--type` names the hash algorithm for a bare input; sui discards it and \
                     infers from length, so the disambiguation the operator paid for is not \
                     applied",
        },
    ),
    (&["hash", "to-base32"], "hash", H),
    (
        &["hash", "to-base32"],
        "type",
        Honour::Refused {
            since: NOW,
            reason: "`--type` names the hash algorithm for a bare input; sui discards it and \
                     infers from length, so the disambiguation the operator paid for is not \
                     applied",
        },
    ),
    (&["hash", "to-base64"], "hash", H),
    (
        &["hash", "to-base64"],
        "type",
        Honour::Refused {
            since: NOW,
            reason: "`--type` names the hash algorithm for a bare input; sui discards it and \
                     infers from length, so the disambiguation the operator paid for is not \
                     applied",
        },
    ),
    (&["hash", "to-sri"], "hash", H),
    (
        &["hash", "to-sri"],
        "type",
        Honour::Refused {
            since: NOW,
            reason: "`--type` names the hash algorithm for a bare input; sui discards it and \
                     infers from length, so the disambiguation the operator paid for is not \
                     applied",
        },
    ),
    // ── key … ───────────────────────────────────────────────────────────
    (&["key", "generate-secret"], "key_name", H),
    // ── registry … ──────────────────────────────────────────────────────
    (&["registry", "list"], "json", H),
    (&["registry", "add"], "from", H),
    (&["registry", "add"], "to", H),
    (&["registry", "remove"], "entry", H),
    (&["registry", "pin"], "entry", H),
];

/// Walk down `matches` following `path`, returning the node for that subcommand
/// — or `None` when the operator did not invoke it.
fn descend<'m>(matches: &'m ArgMatches, path: &[&str]) -> Option<&'m ArgMatches> {
    let mut node = matches;
    for name in path {
        node = node.subcommand_matches(name)?;
    }
    Some(node)
}

/// Refuse every [`Honour::Refused`] argument the operator actually supplied.
///
/// Called ONCE from `main`, before dispatch, so a refusal happens before any
/// side effect — no half-done push, no partly-wiped generation list.
///
/// Only [`ValueSource::CommandLine`] counts. A clap `default_value` populates
/// the match too, and refusing on those would make every defaulted argument
/// trip the gate and the whole CLI unusable.
///
/// # Errors
///
/// Returns [`CliError::NotImplemented`] naming the flag, its subcommand path and
/// the typed reason. `main` renders it and exits **2**.
pub fn enforce(m: &ArgMatches) -> Result<(), CliError> {
    for (path, id, honour) in CONTRACT {
        let Honour::Refused { since, reason } = honour else {
            continue;
        };
        let Some(node) = descend(m, path) else {
            continue;
        };
        if node.value_source(id) != Some(ValueSource::CommandLine) {
            continue;
        }
        let where_ = if path.is_empty() {
            "sui".to_string()
        } else {
            format!("sui {}", path.join(" "))
        };
        return Err(CliError::NotImplemented(format!(
            "`{where_}`: the argument `{id}` is ACCEPTED BUT NOT HONOURED (refused {since}).\n  \
             {reason}.\n  \
             Refusing rather than running and reporting success. See src/cli_contract.rs"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CONTRACT, Honour, enforce};
    use clap::CommandFactory;
    use std::collections::BTreeSet;

    type Key = (Vec<String>, String);

    /// Argument ids clap generates itself and intercepts before dispatch. They
    /// can never reach a handler — `--help` and `--version` print and exit
    /// inside clap — so classifying them would be noise, not coverage.
    const CLAP_INTRINSIC_ARGS: &[&str] = &["help", "version"];

    /// Subcommands clap generates itself (`sui help …`).
    const CLAP_INTRINSIC_SUBCOMMANDS: &[&str] = &["help"];

    /// Look up a `(path, id)` verdict. `None` means unclassified, which
    /// [`every_declared_arg_is_classified`] makes impossible for any declared
    /// argument.
    fn honour_of(path: &[&str], id: &str) -> Option<Honour> {
        CONTRACT
            .iter()
            .find(|(p, i, _)| *p == path && *i == id)
            .map(|(_, _, h)| *h)
    }

    /// Collect every `(subcommand path, arg id)` the BUILT command declares.
    ///
    /// `Command::build()` is what makes this trustworthy: it propagates global
    /// arguments and materializes clap's own `help` / `version`, so the walk
    /// sees the command tree as the parser will, not as the derive wrote it.
    /// Globals are attributed to the ROOT only — after propagation they appear
    /// on every subcommand, and classifying the same flag once per subcommand
    /// would turn one verdict into ~150 copies free to disagree.
    fn declared_args(cmd: &clap::Command, path: &[String], out: &mut BTreeSet<Key>) {
        for arg in cmd.get_arguments() {
            let id = arg.get_id().as_str();
            if CLAP_INTRINSIC_ARGS.contains(&id) {
                continue;
            }
            if arg.is_global_set() && !path.is_empty() {
                continue;
            }
            out.insert((path.to_vec(), id.to_string()));
        }
        for sub in cmd.get_subcommands() {
            let name = sub.get_name();
            if CLAP_INTRINSIC_SUBCOMMANDS.contains(&name) {
                continue;
            }
            let mut child = path.to_vec();
            child.push(name.to_string());
            declared_args(sub, &child, out);
        }
    }

    fn built() -> clap::Command {
        let mut cmd = crate::Cli::command();
        cmd.build();
        cmd
    }

    fn declared() -> BTreeSet<Key> {
        let cmd = built();
        let mut out = BTreeSet::new();
        declared_args(&cmd, &[], &mut out);
        out
    }

    fn contracted() -> BTreeSet<Key> {
        CONTRACT
            .iter()
            .map(|(p, i, _)| {
                (
                    p.iter().map(|s| (*s).to_string()).collect::<Vec<_>>(),
                    (*i).to_string(),
                )
            })
            .collect()
    }

    /// Render a key as a paste-able `CONTRACT` row, so a failure hands the
    /// author the fix instead of a diff to transcribe.
    fn as_row(k: &Key) -> String {
        let path = k
            .0
            .iter()
            .map(|s| format!("\"{s}\""))
            .collect::<Vec<_>>()
            .join(", ");
        format!("    (&[{path}], \"{}\", H),", k.1)
    }

    /// THE TOTALITY PROOF.
    ///
    /// Set-equality in BOTH directions between the arguments clap declares and
    /// the keys `CONTRACT` classifies. A new `#[arg]` nobody classified fails on
    /// the commit that adds it; a stale `CONTRACT` row naming a removed argument
    /// fails too.
    ///
    /// This is a REFLECTION TEST, not a type-level guarantee — see the module
    /// docs. Rust has no lint for "struct field declared and never read"
    /// (`unused_variables` does not fire on `field: _` or `{ .. }`), which is
    /// precisely how ~25 unread flags survived review.
    #[test]
    fn every_declared_arg_is_classified() {
        let declared = declared();
        let contracted = contracted();

        let unclassified: Vec<_> = declared.difference(&contracted).collect();
        let stale: Vec<_> = contracted.difference(&declared).collect();

        assert!(
            unclassified.is_empty() && stale.is_empty(),
            "the CLI flag partition is not total.\n\
             \n\
             {} argument(s) are DECLARED but not classified — every one of them is a flag sui \
             accepts with no stated verdict, which is the exact shape that produced ~25 \
             silently-ignored flags. Add a row to CONTRACT in src/cli_contract.rs:\n{}\n\
             \n\
             {} CONTRACT row(s) name an argument that no longer exists:\n{}\n\
             \n\
             declared={} contracted={}",
            unclassified.len(),
            unclassified
                .iter()
                .map(|k| as_row(k))
                .collect::<Vec<_>>()
                .join("\n"),
            stale.len(),
            stale
                .iter()
                .map(|k| format!("    {k:?}"))
                .collect::<Vec<_>>()
                .join("\n"),
            declared.len(),
            contracted.len(),
        );
    }

    /// The partition is a partition: no `(path, id)` may carry two verdicts.
    #[test]
    fn contract_has_no_duplicate_keys() {
        let mut seen = BTreeSet::new();
        let mut dupes = Vec::new();
        for (p, i, _) in CONTRACT {
            let key: Key = (
                p.iter().map(|s| (*s).to_string()).collect(),
                (*i).to_string(),
            );
            if !seen.insert(key.clone()) {
                dupes.push(key);
            }
        }
        assert!(dupes.is_empty(), "duplicate CONTRACT keys: {dupes:?}");
    }

    /// Report the size of the partition, so the totality claim carries its own
    /// denominator rather than being a bare "it is total".
    #[test]
    fn partition_denominator() {
        let declared = declared();
        let refused = CONTRACT
            .iter()
            .filter(|(_, _, h)| matches!(h, Honour::Refused { .. }))
            .count();
        println!(
            "CLI flag partition: {} declared args classified, {} honoured, {} refused",
            declared.len(),
            CONTRACT.len() - refused,
            refused,
        );
        assert_eq!(declared.len(), CONTRACT.len());
        assert!(refused > 0, "a partition with no refusals is not enforcing");
    }

    /// BAN `hide = true` ON ANY ACCEPTED FLAG.
    ///
    /// Invisible in `--help` AND silently ignored is the worst reachable
    /// combination: undiscoverable, so nobody reports it, and confidently
    /// wrong when someone finds it. If sui accepts a flag it must document it —
    /// including to say the flag is refused, which is information the operator
    /// can act on.
    #[test]
    fn no_accepted_flag_is_hidden() {
        fn walk(cmd: &clap::Command, path: &[String], out: &mut Vec<String>) {
            for arg in cmd.get_arguments() {
                // A hidden GLOBAL is propagated onto all ~129 subcommands, so
                // reporting it per node buries one defect under 129 lines.
                // Attribute it to the root, as the totality walk does.
                if arg.is_global_set() && path.len() > 1 {
                    continue;
                }
                if arg.is_hide_set() {
                    out.push(format!("{} :: {}", path.join(" "), arg.get_id()));
                }
            }
            for sub in cmd.get_subcommands() {
                if CLAP_INTRINSIC_SUBCOMMANDS.contains(&sub.get_name()) {
                    continue;
                }
                let mut child = path.to_vec();
                child.push(sub.get_name().to_string());
                walk(sub, &child, out);
            }
        }
        let cmd = built();
        let mut hidden = Vec::new();
        walk(&cmd, &["sui".to_string()], &mut hidden);
        assert!(
            hidden.is_empty(),
            "these flags are ACCEPTED but hidden from --help — undiscoverable and therefore \
             never reported. Drop `hide = true`; if the flag is not honoured, say so in \
             CONTRACT and in its doc comment:\n{}",
            hidden.join("\n"),
        );
    }

    // ── enforce() behaviour ─────────────────────────────────────────────

    fn matches_from(argv: &[&str]) -> clap::ArgMatches {
        built()
            .try_get_matches_from(argv)
            .unwrap_or_else(|e| panic!("parse {argv:?}: {e}"))
    }

    #[test]
    fn refuses_a_supplied_refused_flag() {
        let m = matches_from(&["sui", "cache", "push", "/nix/store/x", "--cache-url", "http://c"]);
        let err = enforce(&m).expect_err("--cache-url must be refused");
        let msg = err.to_string();
        assert!(msg.contains("cache_url"), "{msg}");
        assert!(msg.contains("sui cache push"), "{msg}");
        assert!(msg.contains("404"), "the reason must survive to the operator: {msg}");
    }

    /// THE `value_source` GUARD, tested rather than asserted.
    ///
    /// `cache push --store-path` carries a clap `default_value`. Were `enforce`
    /// to key on presence instead of source, every defaulted argument would
    /// trip and the CLI would refuse itself. This test fails if that regression
    /// is ever introduced — including via a future row that refuses a defaulted
    /// argument.
    #[test]
    fn a_clap_default_never_trips_the_gate() {
        let m = matches_from(&["sui", "cache", "push", "/nix/store/x"]);
        assert!(
            m.subcommand_matches("cache")
                .and_then(|c| c.subcommand_matches("push"))
                .and_then(|p| p.value_source("store_path"))
                .is_some(),
            "precondition: --store-path must be populated by its default",
        );
        enforce(&m).expect("a defaulted argument must not be refused");
    }

    #[test]
    fn an_unrelated_command_is_untouched() {
        let m = matches_from(&["sui", "store", "ping"]);
        enforce(&m).expect("store ping declares nothing refused");
    }

    /// A refused flag under a subcommand the operator did NOT invoke must not
    /// fire — `descend` returning `None` is the guard.
    #[test]
    fn a_sibling_subcommands_refusal_does_not_fire() {
        let m = matches_from(&["sui", "cache", "info"]);
        enforce(&m).expect("cache info must not inherit cache push's refusal");
    }

    /// A GLOBAL refusal must fire regardless of where the flag sits in argv.
    ///
    /// Globals are keyed at the ROOT in `CONTRACT`, and clap propagates them up
    /// from wherever they were typed — so `sui store paths --option x y` must
    /// refuse exactly as `sui --option x y store paths` does. Without this, the
    /// gate would be a half-gate catching one argv ordering, and the other
    /// ordering is the one people actually type.
    ///
    /// The probe was `--impure` until that flag was correctly reclassified to
    /// Honoured (sui evaluates impurely already, and refusing it broke the
    /// byte-parity gate, whose corpus drives `sui eval --impure`). `--option`
    /// is used instead because it is refused for a reason that will not
    /// evaporate: sui applies no nix settings at all.
    #[test]
    fn a_global_refusal_fires_at_any_argv_position() {
        for argv in [
            &["sui", "--option", "cores", "4", "store", "paths"][..],
            &["sui", "store", "paths", "--option", "cores", "4"][..],
        ] {
            let m = matches_from(argv);
            let err = enforce(&m)
                .expect_err("a refused global must fire from either argv position");
            assert!(err.to_string().contains("option"), "{argv:?}: {err}");
        }
        // …and a two-level subcommand, where propagation has one more hop.
        let m = matches_from(&["sui", "cache", "info", "--quiet"]);
        enforce(&m).expect_err("a global must refuse under a nested subcommand too");
    }

    #[test]
    fn wipe_history_older_than_is_refused() {
        let m = matches_from(&["sui", "profile", "wipe-history", "--older-than", "30d"]);
        let err = enforce(&m).expect_err("--older-than narrows a destructive op; must refuse");
        assert!(err.to_string().contains("older_than"), "{err}");
    }

    #[test]
    fn profile_flag_is_honoured_not_refused() {
        assert_eq!(
            honour_of(&["profile", "install"], "profile"),
            Some(Honour::Honoured),
            "--profile must be IMPLEMENTED: refusing it would punish the explicit operator \
             and keep serving the implicit one who silently hits the default",
        );
        let m = matches_from(&["sui", "profile", "list", "--profile", "/tmp/p"]);
        enforce(&m).expect("--profile is honoured");
    }
}
