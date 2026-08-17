//! Substrate-wide invariant: CLI coverage is a RATCHET, not an equality.
//!
//! ── WHY THIS CHANGED (2026-07-20) ────────────────────────────────────────
//!
//! This file used to assert `replacement_percentage() == 100%` together with
//! `zero_stubs` / `zero_partials` / `zero_missing`. Those four assertions
//! jointly made the `Partial` / `Stub` / `Missing` arms of `SuiCommandMaturity`
//! DEAD BY CI CONSTRUCTION: grading any command honestly turned the build red,
//! so the only two green moves available to an author were "call it Working" or
//! "delete the entry". A type whose honest values fail CI is not a grading
//! surface — it is a pressure to misgrade.
//!
//! That pressure had already been paid. Measured on cid the same day: the
//! catalog reports 100% Working while `sui build --json` discards every flag
//! (`sui/src/main.rs`, the `Commands::Build` arm binds `json: _`) and returns
//! output `darwin-rebuild` cannot consume — `darwin-rebuild` pipes
//! `nix build --json` into `jq -r '.[0].outputs.out'`. A command that breaks the
//! fleet's own rebuild path was graded Working, because Partial was unreachable.
//!
//! ── THE RATCHET ──────────────────────────────────────────────────────────
//!
//! Coverage may not silently DECREASE, and honest grading is a legal move.
//! A real downgrade is recorded by raising the committed baseline below in the
//! same commit that downgrades the entry — one line, reviewable, with the
//! reason in the diff. Silent dilution still fails, which is what the original
//! assertions were reaching for.
//!
//! Tier, not rounded up: a ratchet is a MITIGATION (a CI forcing-function at
//! the C1 ceiling), not a type. It cannot make a wrong grade unrepresentable —
//! only a `cli_coverage`↔`main.rs` bijection can do that, and landing it is the
//! separate invariant `cli_coverage_invariants.rs` already advertises but does
//! not implement.

use sui_spec::cli_coverage::{self, SuiCommandMaturity};

/// The committed ceiling on non-Working entries.
///
/// RAISE THIS in the same commit that honestly downgrades an entry, and say why
/// in the commit message. LOWER IT whenever a gap genuinely closes — that is the
/// ratchet tightening, and it is the only direction that should ever be silent.
// Raised 0 -> 10 (2026-07-22, STRATOSPHERE M3b honesty demotion): ten commands
// catalogued `Working` had handlers that are an unconditional
// `CliError::NotImplemented` (repl, why, path-from-hash-part, edit, log,
// store-diff-closures, upgrade-nix, fmt, print-dev-env, bundle — verified against
// src/main.rs). They are now `Stub` (their honest maturity), so the catalog stops
// over-reporting. Recording a real gap is the ratchet working as intended; LOWER
// this again as each stub becomes a real handler.
// 10 -> 12: also demoted collect-garbage + search to `Partial` — collect-garbage
// only prints a `sui store gc` hint (doesn't collect), and search shells out to
// `nix flake show` (not a native replacement). Both verified against src/main.rs.
// 12 -> 15: demoted `develop`, `store add-file` and `store repair` to `Stub`.
// All three exited 0 while not performing their stated operation, and all three
// are now an unconditional `CliError::NotImplemented` (verified against
// src/main.rs) — the same criterion as the ten demoted above:
//   - `store repair` never repaired. `local=ok` came from `Path::exists()`
//     alone, so a CORRUPT path — the one case the command exists for —
//     reported ok and exited 0.
//   - `store add-file` printed a `/nix/store` path to STDOUT that did not
//     exist, and computed it with the RECURSIVE/NAR hash where `nix store
//     add-file` uses FLAT ingestion, so the path was wrong as well as absent.
//   - `develop` spawned $SHELL with only the STRING attrs of the devShell
//     (dropping `buildInputs`), never sourced `$stdenv/setup`, and set
//     IN_SUI_SHELL rather than IN_NIX_SHELL.
const MAX_NON_WORKING: usize = 15;

/// The committed floor on replacement coverage.
// Lowered 1.0 -> 0.87 (2026-07-22, same honest demotion): true coverage is now
// 67 Working / 77 non-SuiNative = 87.0%, not 100%. This is not a regression in
// sui's behavior — it is the coverage NUMBER becoming truthful. Raise it back
// toward 1.0 as the ten demoted stubs get real implementations.
// Lowered 0.84 -> 0.80 (same honesty demotion as MAX_NON_WORKING 12 -> 15):
// true coverage is now 62 Working / 77 non-SuiNative = 80.5%. Again this is the
// NUMBER becoming truthful, not sui regressing — the three commands behave
// exactly as before minus the false exit 0. Raise it back as they get real
// handlers.
const MIN_REPLACEMENT_PCT: f64 = 0.80;

/// The GAP maturities — the three that mean "sui does not do this yet".
///
/// `SuiNative` is deliberately excluded: it means the command has no `nix`
/// counterpart at all (`sui parity`, `sui fleet deploy`, the whole `store
/// sbom`/`cve-scan` family). Counting it as a gap would make 32 healthy entries
/// look like debt — the first draft of this budget did exactly that, and the
/// test's own failure output is what caught it.
fn gaps() -> Vec<(String, SuiCommandMaturity)> {
    cli_coverage::load_canonical()
        .expect("catalog must load")
        .iter()
        .filter(|c| {
            matches!(
                c.maturity,
                SuiCommandMaturity::Stub
                    | SuiCommandMaturity::Partial
                    | SuiCommandMaturity::Missing
            )
        })
        .map(|c| (c.name.clone(), c.maturity))
        .collect()
}

#[test]
fn replacement_percentage_never_regresses() {
    let pct = cli_coverage::replacement_percentage().expect("catalog must load");
    assert!(
        pct >= MIN_REPLACEMENT_PCT - f64::EPSILON,
        "nix-replacement coverage regressed below the committed floor: now {:.1}%, floor {:.1}%. \
         If this is an HONEST downgrade, lower MIN_REPLACEMENT_PCT in the same commit and say why.",
        pct * 100.0,
        MIN_REPLACEMENT_PCT * 100.0,
    );
}

#[test]
fn gap_entries_stay_within_the_committed_budget() {
    let nw = gaps();
    assert!(
        nw.len() <= MAX_NON_WORKING,
        "Stub/Partial/Missing catalog entries ({}) exceed the committed budget ({}): {:?}\n\
         This is NOT a demand to relabel them Working. If the grades are honest, raise \
         MAX_NON_WORKING in this commit with the reason — recording a real gap is the \
         supported move, and mislabelling to get green is the failure this budget exists \
         to make unnecessary.",
        nw.len(),
        MAX_NON_WORKING,
        nw,
    );
}

#[test]
fn working_command_count_is_stable_or_growing() {
    // Unchanged: a floor, already the right shape. Adding a new Stub/Partial/
    // Missing does not trip this one — the budget test above is what governs
    // dilution now.
    let cat = cli_coverage::load_canonical().unwrap();
    let working = cat
        .iter()
        .filter(|c| c.maturity == SuiCommandMaturity::Working)
        .count();
    // Floor lowered 75 -> 67 (2026-07-22, STRATOSPHERE M3b honest demotion): ten
    // `Working` rows whose handlers are unconditional `NotImplemented` were
    // demoted to `Stub`, so the truthful Working count is 67. This is the number
    // becoming honest, not sui regressing; the floor RATCHETS BACK UP as each
    // demoted stub gets a real handler.
    // Floor lowered 65 -> 62 (honesty demotion of `develop`, `store add-file`
    // and `store repair` — see MAX_NON_WORKING above). Same shape as the
    // 75 -> 67 move: the count becoming honest, not sui regressing.
    assert!(
        working >= 62,
        "working command count regressed: now {working} (floor 62)",
    );
}
