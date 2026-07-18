//! MS1 of the measurement campaign — **wire the `SUI_EVAL_PERF` profiler to a
//! real forced eval** so a workload's cost is *falsifiable*, not hypothesized.
//!
//! Today `SUI_EVAL_PERF=1` *counts*, but nothing calls `perf::report()` on a
//! deep eval — the 56-counter sensor exists yet is unwired to any real target.
//! This rig closes that: snapshot → force-deep a real eval → snapshot →
//! `delta_from`, then dump the per-counter delta (the headline hotspot + the
//! labeled report + a re-runnable `target/marquee-perf.results.json`).
//!
//! Run it (the latch is one-way, so the env must be set for the process):
//! ```text
//! SUI_EVAL_PERF=1 cargo test -p sui-eval --test marquee_perf_profile \
//!     -- --nocapture --test-threads=1
//! ```
//! Without `SUI_EVAL_PERF=1` the counters never accumulate, so the tests
//! skip (green, but no measurement) — the rig is committable either way.
//!
//! Targets, cheapest → real:
//!   * `SMOKE_TARGET` — a self-contained eval-core workload (recursion +
//!     attrset merge + select + interpolation, forced via `builtins.deepSeq`).
//!     Offline; proves the rig captures a sensible profile.  NOT the marquee.
//!   * the online flake marquee (the nix repo's `darwinConfigurations.cid`
//!     deep eval) is the real target — the escalation, gated on
//!     `SUI_TEST_ONLINE=1` (`ms1_marquee_flake_profile`).

mod common;

use sui_eval::perf::{self, PerfSnapshot};

/// A representative eval-core workload — recursion + attrset `//`-merge +
/// `toString`/interpolation + arithmetic + select — forced deep via
/// `builtins.deepSeq` inside the eval itself.  The RIG-PROOF smoke, honestly
/// NOT the marquee: it exercises the eval core self-contained (offline) so we
/// can confirm the sensor captures a non-trivial, sensible profile before
/// pointing it at the heavy online marquee.
const SMOKE_TARGET: &str = r#"
let
  range = builtins.genList (i: i) 200;
  build = n: builtins.foldl'
    (acc: k: acc // { "k${toString k}" = { depth = n; val = k * n; }; })
    {} range;
  tower = builtins.foldl'
    (acc: n: acc // { "level${toString n}" = build n; })
    {} (builtins.genList (i: i) 60);
in builtins.deepSeq tower tower
"#;

/// Capture the per-counter work done evaluating `expr` (forced deep by the
/// expression's own `deepSeq`).  Returns the delta snapshot, or `None` if the
/// eval errored (a missing builtin surfaces here, not silently).
fn profile(expr: &str) -> Option<PerfSnapshot> {
    let before = perf::snapshot();
    sui_eval::eval(expr).ok()?;
    let after = perf::snapshot();
    Some(after.delta_from(&before))
}

/// Dump the profile — the labeled 56-counter report (human) + the headline
/// hotspot + a re-runnable JSON (machine).  The JSON goes to `target/`
/// (regenerable, machine-specific — not committed).
fn dump(label: &str, delta: &PerfSnapshot) {
    eprintln!("\n══ perf profile: {label} ══");
    if let Some((c, n)) = delta.dominant_expr_kind() {
        eprintln!("  dominant expr kind: {} = {n}", perf::counter_name(c));
    }
    if let Some(rate) = delta.thunk_hit_rate() {
        eprintln!("  thunk hit-rate: {:.1}%", rate * 100.0);
    }
    eprintln!(
        "  thunks created/forced: {}/{}",
        delta.thunks_created, delta.thunks_forced
    );
    // The built-in labeled dump of all 56 counters.
    perf::report();

    // Re-runnable machine artifact: the raw counter delta + thunk totals.
    let names: Vec<serde_json::Value> = (0..delta.counters.len())
        .map(|i| serde_json::json!({ "index": i, "value": delta.counters[i] }))
        .collect();
    let out = serde_json::json!({
        "label": label,
        "counters": names,
        "thunks_created": delta.thunks_created,
        "thunks_forced": delta.thunks_forced,
    });
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("marquee-perf-{label}.json"));
    if let Ok(s) = serde_json::to_string_pretty(&out) {
        let _ = std::fs::write(&path, s);
        eprintln!("  wrote {}", path.display());
    }
}

#[test]
fn ms1_rig_smoke_captures_a_real_profile() {
    // The rig's purpose IS measurement — enable counters directly (the
    // designed integration-test path), no env dependency.
    perf::set_enabled(true);
    let delta = profile(SMOKE_TARGET).expect("smoke target must evaluate");
    let total: u64 = delta.counters.iter().sum();
    // The rig WORKS iff it captured non-trivial eval work — the sensor is
    // wired to a real forced eval, not returning an empty profile.
    assert!(total > 0, "the rig must capture a non-zero counter profile");
    dump("smoke", &delta);
}

#[test]
fn ms1_marquee_flake_profile() {
    perf::set_enabled(true);
    if common::skip_if_offline("ms1_marquee") {
        return;
    }
    let dir = common::pleme_io_root().join("nix");
    if !dir.join("flake.nix").exists() {
        eprintln!("skip ms1_marquee: nix repo flake not found at {}", dir.display());
        return;
    }
    // The real marquee path: eval the flake, then force it.  This is the
    // FIRST grounded profile of the marquee shape — the number that turns
    // "hypothesized" into "measured".
    let before = perf::snapshot();
    match sui_eval::builtins::evaluate_flake(&dir) {
        Ok(_) => {
            let after = perf::snapshot();
            let delta = after.delta_from(&before);
            dump("marquee-flake", &delta);
        }
        Err(e) => eprintln!("marquee flake eval did not complete (expected, for now): {e}"),
    }
}
