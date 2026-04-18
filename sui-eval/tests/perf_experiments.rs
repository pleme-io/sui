//! Lisp-authored performance experiments.
//!
//! Same pattern as the oracle corpus, but for declarative perf
//! investigations:
//!
//! ```lisp
//! (defperfexp attrs-size-scaling
//!   :hypothesis
//!     "NixAttrs::get_sym stays ~constant in lookup time as attrset
//!      size grows because FxHashMap dispatches in O(1)."
//!   :variants (
//!     (:name "attrs-5"   :source "let a = { ... }; in a.k2")
//!     (:name "attrs-50"  :source "let a = { ... }; in a.k25")
//!     (:name "attrs-500" :source "let a = { ... }; in a.k250"))
//!   :iterations 1000)
//! ```
//!
//! The harness:
//! 1. Loads every `.lisp` under `tests/perf_corpus/` via
//!    `tatara_lisp::compile_named::<PerfExperimentSpec>`.
//! 2. For each experiment, runs every variant `:iterations` times
//!    through `sui_eval::eval` (and optionally through CppNix if
//!    `SUI_TEST_ONLINE=1`) with a `with_scope` snapshot.
//! 3. Writes a grouped markdown report to
//!    `target/perf-experiments.md` — one section per experiment,
//!    each section a ranked table of variants with µs / eval-work /
//!    dominant-expr / thunk-waste.
//!
//! # Why not just Rust benches?
//!
//! Criterion benches are great for one-off measurements but lousy
//! for *hypothesis-driven investigation*. A perf experiment has a
//! claim ("this should scale O(1)" / "this should be faster after
//! the interner prewarm") that the table either supports or
//! refutes. Authoring experiments in Lisp with a `:hypothesis`
//! field makes that claim a first-class artifact — commit the
//! expected outcome, see it diff when it changes.

mod common;

use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use tatara_lisp::DeriveTataraDomain;

/// One experiment's declaration — a named hypothesis over a set of
/// variant programs. Compiled from `(defperfexp NAME :hypothesis …
/// :variants … :iterations …)` via the tatara-lisp derive.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
#[tatara(keyword = "defperfexp")]
pub struct PerfExperimentSpec {
    /// Plain-English claim the variants are meant to test. Shown in
    /// the report header for each experiment.
    pub hypothesis: String,
    /// The programs to time. Each gets its own row in the
    /// experiment's table.
    pub variants: Vec<PerfVariant>,
    /// How many times to evaluate each variant. Default 100.
    #[serde(default)]
    pub iterations: Option<u32>,
    /// Freeform tags for categorization / filtering. Not consumed
    /// yet by the harness, but available to future drill-downs
    /// (e.g. "run only :tag interner experiments").
    #[serde(default)]
    pub tags: Vec<String>,
}

/// One measurement row. The name shows up in the table; the source
/// is the Nix expression timed verbatim.
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PerfVariant {
    pub name: String,
    pub source: String,
}

/// Load every `(defperfexp …)` form from `tests/perf_corpus/` into a
/// flat list of `NamedDefinition<PerfExperimentSpec>`.
fn load_experiments() -> Vec<tatara_lisp::NamedDefinition<PerfExperimentSpec>> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("perf_corpus");
    if !dir.is_dir() {
        return Vec::new();
    }
    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("perf_corpus: {e}"))
        .filter_map(|e| e.ok().map(|d| d.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("lisp"))
        .collect();
    paths.sort();

    let mut out = Vec::new();
    for path in paths {
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let mut defs =
            tatara_lisp::compile_named::<PerfExperimentSpec>(&src).unwrap_or_else(|e| {
                panic!("compile {}: {e}", path.display())
            });
        for def in &mut defs {
            def.name = format!(
                "{}::{}",
                path.file_stem().and_then(|s| s.to_str()).unwrap_or("?"),
                def.name
            );
        }
        out.extend(defs);
    }
    out
}

/// Time a single variant across `iterations` runs. Returns median
/// Duration + aggregate counter totals (across all iterations) so
/// the report can show both "how fast" and "what did it do".
fn time_variant(source: &str, iterations: u32) -> (Duration, sui_eval::perf::PerfSnapshot) {
    let mut samples: Vec<Duration> = Vec::with_capacity(iterations as usize);
    let (_, snap) = sui_eval::perf::with_scope(|| {
        for _ in 0..iterations {
            let start = Instant::now();
            let _ = sui_eval::eval(source);
            samples.push(start.elapsed());
        }
    });
    samples.sort();
    let median = samples[samples.len() / 2];
    (median, snap)
}

/// Render one experiment's table. Each variant is one row sorted by
/// median µs ascending — fastest variant on top, so "the scaling
/// cost" between rows is readable left-to-right / top-to-bottom.
fn render_experiment(
    def: &tatara_lisp::NamedDefinition<PerfExperimentSpec>,
    results: &[(String, Duration, sui_eval::perf::PerfSnapshot)],
) -> String {
    let mut out = String::new();
    writeln!(out, "### `{}`", def.name).unwrap();
    writeln!(out).unwrap();
    writeln!(out, "> **Hypothesis.** {}", def.spec.hypothesis).unwrap();
    writeln!(out).unwrap();
    if !def.spec.tags.is_empty() {
        writeln!(out, "Tags: {}", def.spec.tags.join(", ")).unwrap();
        writeln!(out).unwrap();
    }
    let it = def.spec.iterations.unwrap_or(100);
    writeln!(out, "Iterations per variant: {it}").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "| variant | median µs | eval_expr | force_value | thunks (cr/fo) | dominant |"
    )
    .unwrap();
    writeln!(
        out,
        "|---------|----------:|----------:|------------:|---------------:|----------|"
    )
    .unwrap();
    let mut sorted: Vec<&(String, Duration, sui_eval::perf::PerfSnapshot)> = results.iter().collect();
    sorted.sort_by(|a, b| a.1.cmp(&b.1));
    for (name, dur, snap) in sorted {
        let dominant = snap
            .dominant_expr_kind()
            .map(|(c, n)| format!("{}({})", sui_eval::perf::counter_name(c), n))
            .unwrap_or_else(|| "-".to_string());
        writeln!(
            out,
            "| `{}` | {} | {} | {} | {}/{} | {} |",
            name,
            dur.as_micros(),
            snap.get(sui_eval::perf::Counter::EvalExpr),
            snap.get(sui_eval::perf::Counter::ForceValue),
            snap.thunks_created,
            snap.thunks_forced,
            dominant,
        )
        .unwrap();
    }
    writeln!(out).unwrap();
    out
}

fn write_report(body: &str) -> std::path::PathBuf {
    let target = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("target");
    std::fs::create_dir_all(&target).ok();
    let path = target.join("perf-experiments.md");
    std::fs::write(&path, body).expect("write perf-experiments.md");
    path
}

#[test]
fn run_all_perf_experiments() {
    let experiments = load_experiments();
    if experiments.is_empty() {
        eprintln!(
            "no experiments under tests/perf_corpus/ — add a `(defperfexp …)` form to run."
        );
        return;
    }

    let mut out = String::new();
    writeln!(out, "# Perf experiments").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "> Regenerated by `cargo test -p sui-eval --test perf_experiments --release`."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Each experiment is one `(defperfexp …)` Lisp form under \
         `tests/perf_corpus/`. Per-variant numbers are medians over \
         the declared iteration count, measured inside a single \
         `sui_eval::perf::with_scope` per variant — counter totals \
         are the SUM across all iterations, so divide by iteration \
         count for per-call numbers."
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "## Experiments").unwrap();
    writeln!(out).unwrap();

    for def in &experiments {
        let iterations = def.spec.iterations.unwrap_or(100);
        let mut results = Vec::with_capacity(def.spec.variants.len());
        for v in &def.spec.variants {
            let (median, snap) = time_variant(&v.source, iterations);
            results.push((v.name.clone(), median, snap));
        }
        out.push_str(&render_experiment(def, &results));
    }

    let path = write_report(&out);
    eprintln!("\nwrote perf experiments report to {}", path.display());
}

#[test]
fn experiment_spec_parses_end_to_end() {
    // Sanity: the derive + loader actually work on a minimal file.
    let src = r#"
        (defperfexp smoke
          :hypothesis "the infrastructure itself is functional"
          :variants (
            (:name "one" :source "1")
            (:name "two" :source "1 + 1"))
          :iterations 10)
    "#;
    let defs = tatara_lisp::compile_named::<PerfExperimentSpec>(src)
        .expect("parse smoke experiment");
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].name, "smoke");
    assert_eq!(defs[0].spec.variants.len(), 2);
    assert_eq!(defs[0].spec.variants[0].name, "one");
    assert_eq!(defs[0].spec.iterations, Some(10));
}
