//! Perf-seal + regression-box: a **work-budget** gate over the sui eval
//! hot path, the speed half of the CONVERGE = SEAL parity contract.
//!
//! # Why this exists
//!
//! `sui parity` (the byte half) proves every eval shape is byte-identical to
//! nix — a *correctness* seal that goes red on a drvPath change. This module is
//! its *speed* peer: a matrix where each eval shape carries a **work budget**,
//! and the gate goes red when a shape starts doing more work than its committed
//! baseline. "Eval got slower" fails CI the same way "drvPath changed" already
//! does — the provable box the operator asked for.
//!
//! # The metric: deterministic WORK, not wall-clock
//!
//! The primary gated metric is a **work counter** (`Counter::EvalExpr` — the
//! number of expression evaluations), read from `sui_eval::perf::PerfSnapshot`
//! via `perf::with_scope`. Work counters are **deterministic across CI
//! runners**: a red row means "this shape now does more eval work", never "the
//! runner was noisy". A tight wall-clock budget on a shared GitHub runner would
//! false-positive and get the gate muted — defeating the whole point — so
//! wall-clock is *reported* but **never gated**.
//!
//! The gated metric is deliberately the ONE stable, committed counter
//! (`EvalExpr`, index 0 — present since perf.rs was born). A finer signal for
//! the overlay `//` re-flatten storm (`OverlayFlattenBuild`) exists in the
//! in-flight byte-parity-wave WIP but is NOT yet committed to sui-eval; wiring
//! the gate to it would couple this box to a volatile counter that the wave
//! removes and re-adds. So the storm-B row is gated on `EvalExpr` (a re-flatten
//! storm inflates total eval work too), and pinning the dedicated
//! `overlay_builds` sub-metric is a named follow-up for when that counter lands
//! committed (tier-honest: the storm is caught coarsely today, finely later).
//!
//! # In-process, on the same expr string
//!
//! Unlike the byte half (which shells out to `sui`/`nix` to diff bytes), the
//! speed half evaluates **in-process** (`sui_eval::eval`) so it can wrap the
//! eval in `perf::with_scope` and read the counters. The expr strings are the
//! same typed-Nix-AST corpus shapes the byte gate uses, plus a few hot-path
//! stress shapes (deep let-scope, overlay re-flatten, deep attr-merge) that
//! target the known eval-time storms.
//!
//! # The ratchet (asymmetric, like Match/GRADUATED)
//!
//! - `work > budget * (1 + tolerance)` → **REGRESSION** — fails the gate.
//! - `work < budget * (1 - tolerance)` → **IMPROVED** — surfaced (warn), does
//!   NOT fail; re-pin the baseline to lock the win in (mirrors how a
//!   `KnownDiverge` that starts matching must GRADUATE to `Match`).
//! - within tolerance → **held**.
//! - a row with no baseline entry → **UNPINNED** — surfaced so a new shape is
//!   never silently un-budgeted (mirrors the byte gate's "new variant without a
//!   matrix row fails the build").

use std::collections::BTreeMap;

use sui_eval::perf::{self, Counter};

/// A speed-gated eval shape: a stable name + the expr to evaluate in-process.
pub struct SpeedRow {
    pub name: &'static str,
    pub expr: String,
    /// One-line description of the eval surface / storm this row guards.
    pub description: &'static str,
}

fn row(name: &'static str, expr: impl Into<String>, description: &'static str) -> SpeedRow {
    SpeedRow {
        name,
        expr: expr.into(),
        description,
    }
}

/// A committed budget for one row: the pinned work-counter value a fix must not
/// regress past. Gated on `EvalExpr` — the one committed, deterministic
/// counter (see the module header on why the finer overlay counter is a
/// deferred follow-up).
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Budget {
    /// Baseline `EvalExpr` count (the primary deterministic work metric).
    pub eval_expr: u64,
}

/// The committed baseline: `name -> Budget`, plus the tolerance the gate
/// applies. A row regresses when its measured work exceeds
/// `budget * (1 + tolerance)`; it has improved when it drops below
/// `budget * (1 - tolerance)`.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Baseline {
    /// Fractional tolerance band (e.g. `0.15` = ±15%). Absorbs the small,
    /// legitimate run-to-run drift of the deterministic counters (allocator
    /// pre-warm, intern-table state) without letting a real storm through.
    pub tolerance: f64,
    /// Per-row pinned budgets, keyed by row name.
    pub rows: BTreeMap<String, Budget>,
}

impl Baseline {
    /// The baseline file lives next to the corpus, committed to the repo, so
    /// "eval got slower" is a `git diff`-detectable change like a drvPath move.
    pub const PATH: &'static str = "src/perf-baseline.json";

    /// Default tolerance when minting a fresh baseline.
    pub const DEFAULT_TOLERANCE: f64 = 0.15;

    fn load() -> Option<Self> {
        let raw = std::fs::read_to_string(Self::PATH).ok()?;
        serde_json::from_str(&raw).ok()
    }

    fn empty() -> Self {
        Self {
            tolerance: Self::DEFAULT_TOLERANCE,
            rows: BTreeMap::new(),
        }
    }
}

/// The measured work for one row, produced by evaluating it in-process under
/// `perf::with_scope`.
pub struct Measured {
    pub name: &'static str,
    pub description: &'static str,
    pub eval_expr: u64,
    pub wall_micros: u128,
    /// `Err` if the in-process eval itself failed (a correctness break the byte
    /// gate would also catch, surfaced here so the speed run never silently
    /// skips a broken shape).
    pub eval_err: Option<String>,
}

/// Per-row verdict against the baseline.
#[derive(Debug, PartialEq, Eq)]
pub enum SpeedVerdict {
    /// Within tolerance of the pinned budget.
    Held,
    /// Work dropped below `budget * (1 - tolerance)` — a real speedup to re-pin.
    Improved,
    /// Work exceeded `budget * (1 + tolerance)` — the gate-failing case.
    Regressed,
    /// No baseline entry — a new shape that must be pinned.
    Unpinned,
    /// The in-process eval errored (surfaced, gate-failing — a broken shape is
    /// not "fast").
    EvalError,
}

/// The eval-shape corpus the speed gate exercises. Pure-eval shapes only (no
/// store realization) so every row runs cheaply in-process under
/// `perf::with_scope`. The set is the storm-targeting union of:
///   1. hot-path stress shapes (deep let-scope, overlay re-flatten, deep
///      attr-merge) that provoke the known eval-time storms, and
///   2. representative shapes lifted from the byte-parity corpus so the two
///      gates cover the same eval surface.
pub fn speed_corpus() -> Vec<SpeedRow> {
    let mut rows = Vec::new();

    // ── STORM A: deep single-name let-scope ───────────────────────────────
    // `is_self_recursive_binding` walks each RHS subtree against every sibling
    // name (O(N²) over a scope of N bindings); the NixOS module fixpoint's
    // deep let-scope (lib/modules.nix declaredConfig/options/matchedOptions/…)
    // is the victim. A 60-binding chained let is the pure-eval stand-in: if the
    // recursiveness classification stops being memoized, EvalExpr blows up
    // super-linearly and this row goes red.
    {
        let n = 60usize;
        let mut binds = String::new();
        for i in 0..n {
            if i == 0 {
                binds.push_str("v0 = 1; ");
            } else {
                binds.push_str(&format!("v{i} = v{} + 1; ", i - 1));
            }
        }
        rows.push(row(
            "storm-A deep let-scope (60 chained bindings)",
            format!("let {binds}in v{}", n - 1),
            "O(N^2) is_self_recursive_binding — the module-fixpoint let-scope storm",
        ));
    }

    // ── STORM B: overlay (`//`) re-flatten under re-derivation ────────────
    // A `//` mints a fresh Overlay node with a cold cache; a fixpoint that
    // re-derives the same `//` gets a cold cache each iteration → O(left+right)
    // re-merge. `OverlayFlattenBuild` would be the finer metric here (it climbs
    // even when EvalExpr is flat), but that counter is not yet committed to
    // sui-eval — so this row gates on EvalExpr, which a re-flatten storm
    // inflates too via the extra force/merge work. Pinning the dedicated
    // overlay sub-metric is the named follow-up (module header).
    {
        let mut base = String::from("{ ");
        for i in 0..20 {
            base.push_str(&format!("b{i} = {i}; "));
        }
        base.push('}');
        let mut expr = base.clone();
        for i in 0..40 {
            expr = format!("({expr} // {{ k{i} = {i}; }})");
        }
        // `attrNames` forces a full flatten of the whole overlay chain (a
        // single `.kN` select would short-circuit via `get_sym` without
        // flattening).
        rows.push(row(
            "storm-B overlay re-flatten (40-deep // chain, attrNames)",
            format!("builtins.length (builtins.attrNames ({expr}))"),
            "// re-flatten storm — full flatten of a deep // chain",
        ));
    }

    // ── STORM C/E: deep attr merge (dotted + full-set collision) ──────────
    // The dotted/full-set deep-merge path (pkg-config-wrapper env.addFlags
    // shape) exercised at depth. Guards the flatten+merge work of the merge
    // path itself.
    {
        let mut entries = String::new();
        for i in 0..30 {
            entries.push_str(&format!("a.k{i} = {i}; "));
        }
        // a colliding full-set write forces the deep-merge branch.
        entries.push_str("a = { extra = 99; }; ");
        rows.push(row(
            "storm-C deep attr merge (30 dotted + full-set collision)",
            format!("let s = {{ {entries}}}; in builtins.length (builtins.attrNames s.a)"),
            "dotted+full-set deep-merge work volume",
        ));
    }

    // ── representative pure-eval byte-corpus shapes ───────────────────────
    // These mirror the byte gate's surface so the speed gate covers the same
    // eval shapes the correctness gate does.
    rows.push(row(
        "attr-merge order1 (dotted then fullset)",
        r#"let s = { a.b = "x"; a = { c = "y"; }; }; in s.a.b + s.a.c"#,
        "dotted-then-fullset merge (root 73b904d)",
    ));
    rows.push(row(
        "with-namespace laziness (attrNames over with-throw)",
        r#"builtins.attrNames (with (throw "X"); { a = 1; b = 2; })"#,
        "with-namespace stays lazy (M2.6 root #4a)",
    ));
    rows.push(row(
        "dotted full-set leaf deep-merge",
        "builtins.attrNames ({ o.a = { x = 1; }; o.a.y = 2; }.o.a)",
        "dotted full-set leaf deep-merge (M2.6 root #4b)",
    ));
    rows.push(row(
        "foldl' sum over range (arith hot loop)",
        "builtins.foldl' (acc: x: acc + x) 0 (builtins.genList (i: i) 500)",
        "arithmetic/apply hot loop — genList + foldl'",
    ));
    rows.push(row(
        "recursive attrset fixpoint (rec self-reference)",
        "let s = rec { a = 1; b = a + 1; c = b + a; }; in s.c",
        "rec-attrset self-reference resolution",
    ));

    rows
}

/// Evaluate one row in-process under `perf::with_scope`, capturing the work
/// snapshot. Gated on the deterministic `EvalExpr` counter, plus a
/// reported-only wall-clock.
fn measure(r: &SpeedRow) -> Measured {
    let start = std::time::Instant::now();
    let (result, snap) = perf::with_scope(|| sui_eval::eval(&r.expr));
    let wall_micros = start.elapsed().as_micros();
    let eval_err = result.err().map(|e| e.to_string());
    Measured {
        name: r.name,
        description: r.description,
        eval_expr: snap.get(Counter::EvalExpr),
        wall_micros,
        eval_err,
    }
}

/// Grade one measured row against the baseline + tolerance.
fn grade(m: &Measured, baseline: &Baseline) -> SpeedVerdict {
    if m.eval_err.is_some() {
        return SpeedVerdict::EvalError;
    }
    let Some(budget) = baseline.rows.get(m.name) else {
        return SpeedVerdict::Unpinned;
    };
    let tol = baseline.tolerance;
    #[allow(clippy::cast_precision_loss)]
    let (measured, pinned) = (m.eval_expr as f64, budget.eval_expr as f64);
    if measured > pinned * (1.0 + tol) {
        SpeedVerdict::Regressed
    } else if measured < pinned * (1.0 - tol) {
        SpeedVerdict::Improved
    } else {
        SpeedVerdict::Held
    }
}

/// Run the whole speed gate. When `write_baseline` is set, (re)mint the
/// committed baseline from the current measurements instead of grading — used
/// to pin a fresh baseline or lock in a proven speedup. Otherwise grade against
/// the committed baseline and exit non-zero on any regression / eval-error /
/// unpinned row (the sealed box).
pub fn run(json: bool, write_baseline: bool) -> Result<(), sui::CliError> {
    // Counters must be on before with_scope resets/reads them.
    perf::set_enabled(true);

    let corpus = speed_corpus();
    let measured: Vec<Measured> = corpus.iter().map(measure).collect();

    if write_baseline {
        let mut base = Baseline::empty();
        for m in &measured {
            if let Some(err) = &m.eval_err {
                return Err(sui::CliError::NotImplemented(format!(
                    "perf-seal --write-baseline: row {:?} does not evaluate: {err}",
                    m.name
                )));
            }
            base.rows.insert(
                m.name.to_string(),
                Budget {
                    eval_expr: m.eval_expr,
                },
            );
        }
        let text = serde_json::to_string_pretty(&base)
            .map_err(|e| sui::CliError::NotImplemented(format!("serialize baseline: {e}")))?;
        std::fs::write(Baseline::PATH, format!("{text}\n"))
            .map_err(|e| sui::CliError::NotImplemented(format!("write baseline: {e}")))?;
        if json {
            println!(
                "{}",
                serde_json::json!({"action": "write-baseline", "path": Baseline::PATH, "rows": base.rows.len()})
            );
        } else {
            println!(
                "wrote {} rows to {} (tolerance ±{:.0}%)",
                base.rows.len(),
                Baseline::PATH,
                base.tolerance * 100.0
            );
        }
        return Ok(());
    }

    let baseline = Baseline::load().unwrap_or_else(Baseline::empty);
    let verdicts: Vec<(usize, SpeedVerdict)> = measured
        .iter()
        .enumerate()
        .map(|(i, m)| (i, grade(m, &baseline)))
        .collect();

    let regressed = verdicts
        .iter()
        .filter(|(_, v)| *v == SpeedVerdict::Regressed)
        .count();
    let improved = verdicts
        .iter()
        .filter(|(_, v)| *v == SpeedVerdict::Improved)
        .count();
    let unpinned = verdicts
        .iter()
        .filter(|(_, v)| *v == SpeedVerdict::Unpinned)
        .count();
    let eval_errors = verdicts
        .iter()
        .filter(|(_, v)| *v == SpeedVerdict::EvalError)
        .count();
    // The sealed box: a regression, an eval-error, or an unpinned (un-budgeted)
    // shape all fail the gate — the speed peer of the byte gate's `unexpected`.
    let failed = regressed + eval_errors + unpinned;

    if json {
        let rows: Vec<serde_json::Value> = measured
            .iter()
            .zip(&verdicts)
            .map(|(m, (_, v))| {
                let budget = baseline.rows.get(m.name);
                serde_json::json!({
                    "name": m.name,
                    "description": m.description,
                    "eval_expr": m.eval_expr,
                    "wall_micros": m.wall_micros,
                    "budget_eval_expr": budget.map(|b| b.eval_expr),
                    "verdict": verdict_label(v),
                    "eval_err": m.eval_err,
                })
            })
            .collect();
        let summary = serde_json::json!({
            "tolerance": baseline.tolerance,
            "total": measured.len(),
            "regressed": regressed,
            "improved": improved,
            "unpinned": unpinned,
            "eval_errors": eval_errors,
            "failed": failed,
            "rows": rows,
        });
        println!("{}", serde_json::to_string_pretty(&summary).unwrap());
    } else {
        print_table(&measured, &verdicts, &baseline);
    }

    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn verdict_label(v: &SpeedVerdict) -> &'static str {
    match v {
        SpeedVerdict::Held => "held",
        SpeedVerdict::Improved => "improved",
        SpeedVerdict::Regressed => "REGRESSED",
        SpeedVerdict::Unpinned => "UNPINNED",
        SpeedVerdict::EvalError => "EVAL-ERROR",
    }
}

fn print_table(measured: &[Measured], verdicts: &[(usize, SpeedVerdict)], baseline: &Baseline) {
    use sui_spec::style::{body, error, glyph_snowflake, header, ident, info, muted, success, warn};

    println!(
        "{}  {}  ({} shapes, work-budget ±{:.0}%)",
        glyph_snowflake(),
        header("sui eval perf-seal"),
        ident(&measured.len().to_string()),
        baseline.tolerance * 100.0
    );
    println!();
    let name_w = measured.iter().map(|m| m.name.len()).max().unwrap_or(20);
    for (m, (_, v)) in measured.iter().zip(verdicts) {
        let budget = baseline.rows.get(m.name).map(|b| b.eval_expr);
        let (glyph, label) = match v {
            SpeedVerdict::Held => (success("✓"), success("held")),
            SpeedVerdict::Improved => (warn("↓"), warn("IMPROVED → re-pin baseline")),
            SpeedVerdict::Regressed => (error("✘"), error("REGRESSED")),
            SpeedVerdict::Unpinned => (warn("+"), warn("UNPINNED → add to baseline")),
            SpeedVerdict::EvalError => (error("✘"), error("EVAL-ERROR")),
        };
        let budget_txt = budget
            .map(|b| format!("budget:{b}"))
            .unwrap_or_else(|| "budget:—".to_string());
        #[allow(clippy::cast_precision_loss)]
        let wall_ms = m.wall_micros as f64 / 1000.0;
        println!(
            "  {} {}  eval:{:<8} {}  {:.2}ms  {}",
            glyph,
            ident(&format!("{:<name_w$}", m.name, name_w = name_w)),
            m.eval_expr,
            muted(&budget_txt),
            wall_ms,
            label,
        );
        if !m.description.is_empty() {
            println!("      {}  {}", muted("·"), info(m.description));
        }
        if let Some(err) = &m.eval_err {
            println!("      {}  {}", error("✘"), error(err));
        }
    }
    println!();
    let regressed = verdicts
        .iter()
        .filter(|(_, v)| *v == SpeedVerdict::Regressed || *v == SpeedVerdict::EvalError)
        .count();
    let improved = verdicts
        .iter()
        .filter(|(_, v)| *v == SpeedVerdict::Improved)
        .count();
    let unpinned = verdicts
        .iter()
        .filter(|(_, v)| *v == SpeedVerdict::Unpinned)
        .count();
    println!(
        "  {} {} regressed · {} improved · {} unpinned",
        body("∑"),
        if regressed > 0 {
            error(&regressed.to_string())
        } else {
            success("0")
        },
        if improved > 0 {
            warn(&improved.to_string())
        } else {
            muted("0")
        },
        if unpinned > 0 {
            warn(&unpinned.to_string())
        } else {
            muted("0")
        },
    );
    if regressed == 0 && unpinned == 0 {
        println!(
            "  {} perf sealed — every shape within its work budget",
            success("✔")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_speed_row_evaluates() {
        // The corpus must be all-pure-eval: every row evaluates in-process
        // without error. A row that can't eval has no business carrying a work
        // budget (the storm it guards would be un-measurable).
        perf::set_enabled(true);
        for r in speed_corpus() {
            let m = measure(&r);
            assert!(
                m.eval_err.is_none(),
                "speed row {:?} failed to eval: {:?}",
                r.name,
                m.eval_err
            );
            assert!(
                m.eval_expr > 0,
                "speed row {:?} recorded zero eval work — counters not capturing",
                r.name
            );
        }
    }

    #[test]
    fn row_names_are_unique() {
        // Names are the baseline keys — a collision would silently merge two
        // shapes' budgets (CATALOG-REFLECTION: keys must be unique).
        let corpus = speed_corpus();
        let mut seen = std::collections::BTreeSet::new();
        for r in &corpus {
            assert!(seen.insert(r.name), "duplicate speed-row name: {:?}", r.name);
        }
    }

    #[test]
    fn grade_regresses_when_over_budget() {
        let mut base = Baseline::empty();
        base.tolerance = 0.10;
        base.rows.insert("x".to_string(), Budget { eval_expr: 100 });
        let over = Measured {
            name: "x",
            description: "",
            eval_expr: 200,
            wall_micros: 0,
            eval_err: None,
        };
        assert_eq!(grade(&over, &base), SpeedVerdict::Regressed);
        let held = Measured {
            eval_expr: 105,
            ..over_like("x")
        };
        assert_eq!(grade(&held, &base), SpeedVerdict::Held);
        let improved = Measured {
            eval_expr: 50,
            ..over_like("x")
        };
        assert_eq!(grade(&improved, &base), SpeedVerdict::Improved);
    }

    #[test]
    fn grade_flags_unpinned_and_eval_error() {
        let base = Baseline::empty();
        let unpinned = over_like("new-shape");
        assert_eq!(grade(&unpinned, &base), SpeedVerdict::Unpinned);
        let broken = Measured {
            eval_err: Some("boom".into()),
            ..over_like("x")
        };
        assert_eq!(grade(&broken, &base), SpeedVerdict::EvalError);
    }

    fn over_like(name: &'static str) -> Measured {
        Measured {
            name,
            description: "",
            eval_expr: 0,
            wall_micros: 0,
            eval_err: None,
        }
    }
}
