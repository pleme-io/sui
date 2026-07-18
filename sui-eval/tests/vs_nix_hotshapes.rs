//! Head-to-head performance measurement: sui (tree-walker) vs CppNix on
//! the closure/thunk/attrset/recursion **hot shapes**, plus a drvPath
//! byte-parity correctness cross-check.
//!
//! # Why this test exists
//!
//! `vs_cppnix.rs` measures the whole oracle corpus (breadth). This test
//! is the **re-runnable ratio harness** for the specific evaluator hot
//! shapes the #1 optimization targets — recursion, list folds/maps,
//! let-chains, attrset select, overlay flatten, closure fixpoints. It
//! serializes a typed `results.json` so the unbacked "3× CppNix"
//! headline becomes a *measured number* an operator can cite.
//!
//! For every shape we run the same Nix source through:
//!
//! 1. `sui_eval::eval` — in-process, no IPC, the tree-walker (the
//!    byte-parity-correct engine; the VM defers string-context and is
//!    NOT parity-capable, so measurement rides the tree-walker).
//! 2. `nix-instantiate --eval --json --strict -E "$src"` — out-of-
//!    process, includes a `fork+exec` spawn floor.
//!
//! # Honesty (identical envelope to vs_cppnix.rs)
//!
//! The CppNix wall time includes process-spawn overhead a persistent
//! daemon wouldn't pay. On tiny shapes the spawn floor dominates, so a
//! large `wall_ratio` is mostly spawn cost, NOT engine delta. We surface
//! two ratios:
//!
//! - `wall_ratio`   = nix_wall_us / sui_us   (user-perspective)
//! - `engine_ratio` = nix_eval_us / sui_us   (engine delta; nix_eval =
//!   nix_wall − spawn_floor). Only meaningful when
//!   `nix_eval_us ≥ MEANINGFUL_CPPNIX_EVAL_US`; shapes below that are
//!   marked `engine_comparable = false` and excluded from the engine
//!   geomean.
//!
//! **`engine_ratio` on the comparable shapes is the honest headline
//! column.** `wall_ratio` overstates the win on shapes too fast to
//! differentiate.
//!
//! # Opt-in
//!
//! Requires `SUI_TEST_ONLINE=1` (same gate as the differential oracle);
//! skips silently when `nix-instantiate` isn't on PATH. Not a CI gate —
//! human review of `results.json` + the `.md` mirror is the feedback
//! loop. The drvPath correctness section DOES hard-assert byte-equality:
//! this harness doubles as a parity gate.

mod common;

use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use serde::Serialize;

const ITERATIONS: usize = 7;

/// Shapes below this estimated pure-eval cost (µs) are spawn-noise —
/// both engines finish before the fork+exec floor is even paid, so
/// subtracting the floor leaves measurement noise, not an engine delta.
/// Identical threshold to `vs_cppnix.rs::MEANINGFUL_CPPNIX_EVAL_US`.
const MEANINGFUL_CPPNIX_EVAL_US: u128 = 500;

// ── Hot-shape corpus ────────────────────────────────────────────────────
//
// Each row is (class, name, expr). The fixed exprs live here; two are
// generated in Rust (deep_let_100, big_attrset_50) and appended at
// runtime so the source stays honest (no hand-transcribed 100-line
// strings). These exercise the closure/thunk/attrset/recursion hot
// shapes and are small enough to run in seconds.

const HOT_SHAPES: &[(&str, &str, &str)] = &[
    (
        "recursion",
        "rec_fib_10",
        "let f = n: if n < 2 then n else f (n - 1) + f (n - 2); in f 10",
    ),
    (
        "recursion-deep",
        "rec_fib_20",
        "let f = n: if n < 2 then n else f (n - 1) + f (n - 2); in f 20",
    ),
    (
        "list-fold",
        "list_foldl_100",
        "builtins.foldl' (acc: x: acc + x) 0 (builtins.genList (x: x) 100)",
    ),
    (
        "list-map",
        "list_map_20",
        "builtins.map (x: x * x) (builtins.genList (x: x) 20)",
    ),
    (
        "let-chain",
        "let_5",
        "let a = 1; b = a + 1; c = b + 1; d = c + 1; e = d + 1; in e",
    ),
    (
        "overlay-flatten",
        "attrset_merge",
        "{ a = 1; b = 2; c = 3; } // { b = 20; d = 4; } // { e = 5; }",
    ),
    (
        "closure-fixpoint",
        "rec_attr_fib",
        // NB: spaces around `-` are load-bearing. sui's tree-walker parses
        // `s.f (n-1)` (no spaces) as a reference to a variable literally
        // named `n-1` and fails with `UndefinedVar("'n-1'")`, whereas
        // CppNix parses it as `n - 1`. A shape only one engine can eval
        // yields no ratio, so the measurement shape is spaced. (The
        // no-space parse divergence is a genuine sui gap, noted here, not a
        // harness bug.)
        "let s = { f = n: if n < 2 then n else s.f (n - 1) + s.f (n - 2); }; in s.f 12",
    ),
];

/// Build the 100-binding let chain:
/// `let x_00 = 0; x_01 = x_00 + 1; … x_99 = x_98 + 1; in x_99`.
fn gen_deep_let_100() -> String {
    let mut s = String::from("let x_00 = 0; ");
    for i in 1..100 {
        // reference the previous binding
        let _ = write!(s, "x_{i:02} = x_{prev:02} + 1; ", prev = i - 1);
    }
    s.push_str("in x_99");
    s
}

/// Build the 50-key attrset select:
/// `let attrs = { key_00 = 0; … key_49 = 49; }; in attrs.key_00 + attrs.key_25 + attrs.key_49`.
fn gen_big_attrset_50() -> String {
    let mut s = String::from("let attrs = { ");
    for i in 0..50 {
        let _ = write!(s, "key_{i:02} = {i}; ");
    }
    s.push_str("}; in attrs.key_00 + attrs.key_25 + attrs.key_49");
    s
}

/// The full shape set: the fixed table plus the two Rust-generated
/// large shapes, returned as owned `(class, name, expr)` triples.
fn all_shapes() -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = HOT_SHAPES
        .iter()
        .map(|(c, n, e)| ((*c).to_string(), (*n).to_string(), (*e).to_string()))
        .collect();
    out.push((
        "let-chain-deep".to_string(),
        "deep_let_100".to_string(),
        gen_deep_let_100(),
    ));
    out.push((
        "attrset-select".to_string(),
        "big_attrset_50".to_string(),
        gen_big_attrset_50(),
    ));
    out
}

// ── Timing (reused shape from vs_cppnix.rs) ──────────────────────────────

/// Eval `src` via `sui_eval::eval` (tree-walker, in-process) — wall
/// time on success, `None` on failure.
fn time_sui(src: &str) -> Option<Duration> {
    let start = Instant::now();
    let out = sui_eval::eval(src);
    let elapsed = start.elapsed();
    out.ok().map(|_| elapsed)
}

/// Eval `src` via `nix-instantiate --eval --json --strict -E src` — wall
/// time on success, `None` on failure. Wraps a leading-`-` expr in
/// `(expr)` so it isn't parsed as a flag (same guard as vs_cppnix.rs).
fn time_cppnix(src: &str) -> Option<Duration> {
    let wrapped;
    let passed: &str = if src.trim_start().starts_with('-') {
        wrapped = format!("({src})");
        &wrapped
    } else {
        src
    };
    let start = Instant::now();
    let output = Command::new("nix-instantiate")
        .args(["--eval", "--json", "--strict", "-E", passed])
        .output()
        .ok()?;
    let elapsed = start.elapsed();
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let _parsed: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    Some(elapsed)
}

/// Median of a small sample (len > 0). Lower-mid, not averaged — same
/// as vs_cppnix.rs.
fn median(mut xs: Vec<Duration>) -> Duration {
    xs.sort();
    xs[xs.len() / 2]
}

/// Measure the irreducible `nix-instantiate` spawn floor from evaluating
/// a trivial expression. Subtracted from each cppnix measurement to
/// estimate pure-eval cost. Same as vs_cppnix.rs.
fn measure_cppnix_spawn_floor() -> Duration {
    let samples: Vec<Duration> = (0..7).filter_map(|_| time_cppnix("1")).collect();
    if samples.is_empty() {
        return Duration::ZERO;
    }
    median(samples)
}

// ── Typed result shapes (serialized via serde, NO format!() of JSON) ─────

#[derive(Serialize)]
struct ShapeResult {
    class: String,
    name: String,
    sui_us: u128,
    nix_wall_us: u128,
    nix_eval_us: u128,
    wall_ratio: f64,
    engine_ratio: f64,
    engine_comparable: bool,
    source: String,
}

#[derive(Serialize)]
struct CorrectnessResult {
    expr: String,
    sui_drv: String,
    nix_drv: String,
    #[serde(rename = "match")]
    matched: bool,
}

#[derive(Serialize)]
struct Summary {
    wall_geomean: f64,
    engine_geomean: f64,
    n: usize,
    n_engine_comparable: usize,
}

#[derive(Serialize)]
struct Results {
    engine: String,
    nix_version: String,
    sui_version: String,
    profile: String,
    spawn_floor_us: u128,
    shapes: Vec<ShapeResult>,
    summary: Summary,
    correctness: Vec<CorrectnessResult>,
}

fn geomean(ratios: &[f64]) -> f64 {
    if ratios.is_empty() {
        return 0.0;
    }
    let log_sum: f64 = ratios.iter().map(|r| r.max(1e-6).ln()).sum();
    (log_sum / ratios.len() as f64).exp()
}

fn nix_version() -> String {
    Command::new("nix-instantiate")
        .arg("--version")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn target_dir() -> PathBuf {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root exists")
        .join("target");
    std::fs::create_dir_all(&p).ok();
    p
}

// ── drvPath correctness cross-check ──────────────────────────────────────
//
// For each (label, expr) we eval `(pkg).drvPath` through:
//   sui --no-vm eval --raw --impure -E EXPR   (SUBPROCESS → store machinery)
//   nix eval --raw --impure --expr EXPR
// and record byte-equality. A mismatch FAILS the test — this section
// doubles as a parity gate on the store-path machinery.
//
// `--no-vm` selects sui's tree-walker (the parity-correct engine). It is
// a GLOBAL sui flag; `--raw`/`-E` are the `eval` subcommand flags.

/// getFlake-based pkg drvPath expressions. If getFlake nixpkgs isn't
/// resolvable at runtime, the correctness section is skipped (recorded
/// empty) rather than failing — the perf table stands on its own.
const CORRECTNESS_PKGS: &[(&str, &str)] = &[
    (
        "hello",
        "(import (builtins.getFlake \"nixpkgs\") { system = builtins.currentSystem; }).hello.drvPath",
    ),
    (
        "coreutils",
        "(import (builtins.getFlake \"nixpkgs\") { system = builtins.currentSystem; }).coreutils.drvPath",
    ),
];

/// Eval a `.drvPath` expr through the sui CLI subprocess (tree-walker).
fn sui_drv_subprocess(expr: &str) -> Option<String> {
    let out = Command::new("sui")
        .args(["--no-vm", "eval", "--raw", "--impure", "-E", expr])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // sui prints a `[census exit] …` line after the value on some
    // builds; the drvPath is the first line that looks like a store path.
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("/nix/store/") && l.ends_with(".drv"))
        .map(str::to_string)
}

/// Eval a `.drvPath` expr through real `nix eval --raw`.
fn nix_drv_subprocess(expr: &str) -> Option<String> {
    let out = Command::new("nix")
        .args([
            "eval",
            "--raw",
            "--impure",
            "--extra-experimental-features",
            "nix-command flakes",
            "--expr",
            expr,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    // nix prints warnings on stderr, the raw value on stdout.
    let v = stdout.trim();
    if v.starts_with("/nix/store/") && v.ends_with(".drv") {
        Some(v.to_string())
    } else {
        None
    }
}

/// Run the drvPath cross-check. Returns the recorded results; hard-panics
/// on a genuine byte mismatch (both engines produced a store path but
/// they differ). Skips a pkg silently if either engine can't resolve it
/// (e.g. getFlake nixpkgs unavailable), recording nothing for it.
fn run_correctness() -> Vec<CorrectnessResult> {
    let mut out = Vec::new();
    for (label, expr) in CORRECTNESS_PKGS {
        let sui = sui_drv_subprocess(expr);
        let nix = nix_drv_subprocess(expr);
        match (sui, nix) {
            (Some(sui_drv), Some(nix_drv)) => {
                let matched = sui_drv == nix_drv;
                assert!(
                    matched,
                    "drvPath BYTE MISMATCH on {label}:\n  sui: {sui_drv}\n  nix: {nix_drv}"
                );
                out.push(CorrectnessResult {
                    expr: (*expr).to_string(),
                    sui_drv,
                    nix_drv,
                    matched,
                });
            }
            _ => {
                eprintln!(
                    "correctness: skip {label} (getFlake nixpkgs or engine unavailable)"
                );
            }
        }
    }
    out
}

// ── Report ───────────────────────────────────────────────────────────────

fn render_md(r: &Results) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# sui (tree-walker) vs CppNix — hot-shape ratio harness");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "> Regenerated by `SUI_TEST_ONLINE=1 cargo test -p sui-eval --test vs_nix_hotshapes -- --nocapture`."
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "## Environment");
    let _ = writeln!(out);
    let _ = writeln!(out, "| field | value |");
    let _ = writeln!(out, "|-------|-------|");
    let _ = writeln!(out, "| engine | {} |", r.engine);
    let _ = writeln!(out, "| nix | {} |", r.nix_version);
    let _ = writeln!(out, "| sui | {} |", r.sui_version);
    let _ = writeln!(out, "| profile | {} |", r.profile);
    let _ = writeln!(out, "| cppnix spawn floor | {} µs |", r.spawn_floor_us);
    let _ = writeln!(out);
    let _ = writeln!(out, "## Summary");
    let _ = writeln!(out);
    let _ = writeln!(out, "| metric | value |");
    let _ = writeln!(out, "|--------|------:|");
    let _ = writeln!(out, "| shapes | {} |", r.summary.n);
    let _ = writeln!(
        out,
        "| engine-comparable (nix_eval ≥ {MEANINGFUL_CPPNIX_EVAL_US} µs) | {} |",
        r.summary.n_engine_comparable
    );
    let _ = writeln!(
        out,
        "| **wall-clock geomean (sui vs nix-instantiate)** | **{:.1}×** |",
        r.summary.wall_geomean
    );
    let _ = writeln!(
        out,
        "| **engine geomean (comparable shapes — the honest column)** | **{:.2}×** |",
        r.summary.engine_geomean
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "## Per-shape");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "`engine_ratio` is `-` where the shape is too fast to differentiate (spawn-noise)."
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "| class | shape | sui µs | nix_wall µs | nix_eval µs | wall× | engine× | comparable |"
    );
    let _ = writeln!(
        out,
        "|-------|-------|------:|------------:|------------:|------:|--------:|:----------:|"
    );
    let mut shapes: Vec<&ShapeResult> = r.shapes.iter().collect();
    shapes.sort_by(|a, b| a.name.cmp(&b.name));
    for s in shapes {
        let eng = if s.engine_comparable {
            format!("{:.2}×", s.engine_ratio)
        } else {
            "-".to_string()
        };
        let _ = writeln!(
            out,
            "| {} | `{}` | {} | {} | {} | {:.1}× | {} | {} |",
            s.class,
            s.name,
            s.sui_us,
            s.nix_wall_us,
            s.nix_eval_us,
            s.wall_ratio,
            eng,
            if s.engine_comparable { "yes" } else { "no" },
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## drvPath correctness cross-check (byte-parity gate)");
    let _ = writeln!(out);
    if r.correctness.is_empty() {
        let _ = writeln!(
            out,
            "*(skipped — getFlake nixpkgs / a store engine was unavailable at run time.)*"
        );
    } else {
        let _ = writeln!(out, "| pkg | match | drvPath |");
        let _ = writeln!(out, "|-----|:-----:|---------|");
        for c in &r.correctness {
            // derive a short label from the expr
            let label = c
                .expr
                .split(").")
                .last()
                .and_then(|s| s.strip_suffix(".drvPath"))
                .unwrap_or("pkg");
            let _ = writeln!(
                out,
                "| {} | {} | `{}` |",
                label,
                if c.matched { "✓" } else { "✗" },
                c.sui_drv
            );
        }
    }
    out
}

#[test]
fn compare_sui_vs_nix_hotshapes() {
    if common::skip_if_offline("compare_sui_vs_nix_hotshapes") {
        return;
    }

    eprintln!("measuring CppNix spawn floor…");
    let spawn_floor = measure_cppnix_spawn_floor();
    let spawn_floor_us = spawn_floor.as_micros();
    eprintln!("CppNix spawn floor: {spawn_floor_us} µs");

    let shapes = all_shapes();
    let mut results: Vec<ShapeResult> = Vec::with_capacity(shapes.len());

    for (class, name, expr) in &shapes {
        // sui samples
        let mut sui_samples: Vec<Duration> = Vec::with_capacity(ITERATIONS);
        for _ in 0..ITERATIONS {
            if let Some(d) = time_sui(expr) {
                sui_samples.push(d);
            }
        }
        if sui_samples.is_empty() {
            eprintln!("sui failed to eval {name}; skipping");
            continue;
        }

        // cppnix samples
        let mut nix_samples: Vec<Duration> = Vec::with_capacity(ITERATIONS);
        for _ in 0..ITERATIONS {
            if let Some(d) = time_cppnix(expr) {
                nix_samples.push(d);
            }
        }
        if nix_samples.is_empty() {
            eprintln!("nix failed to eval {name}; skipping");
            continue;
        }

        let sui_us = median(sui_samples).as_micros().max(1);
        let nix_wall_us = median(nix_samples).as_micros();
        let nix_eval_us = nix_wall_us.saturating_sub(spawn_floor_us);
        #[allow(clippy::cast_precision_loss)]
        let wall_ratio = nix_wall_us.max(1) as f64 / sui_us as f64;
        #[allow(clippy::cast_precision_loss)]
        let engine_ratio = nix_eval_us.max(1) as f64 / sui_us as f64;
        let engine_comparable = nix_eval_us >= MEANINGFUL_CPPNIX_EVAL_US;

        results.push(ShapeResult {
            class: class.clone(),
            name: name.clone(),
            sui_us,
            nix_wall_us,
            nix_eval_us,
            wall_ratio,
            engine_ratio,
            engine_comparable,
            source: expr.clone(),
        });
    }

    assert!(!results.is_empty(), "no shapes measured — both engines failed");

    // Geomeans
    let wall_ratios: Vec<f64> = results.iter().map(|s| s.wall_ratio).collect();
    let engine_ratios: Vec<f64> = results
        .iter()
        .filter(|s| s.engine_comparable)
        .map(|s| s.engine_ratio)
        .collect();
    let n_engine_comparable = engine_ratios.len();
    let summary = Summary {
        wall_geomean: geomean(&wall_ratios),
        engine_geomean: geomean(&engine_ratios),
        n: results.len(),
        n_engine_comparable,
    };

    // Correctness (hard-asserts byte-equality on any resolvable pkg)
    let correctness = run_correctness();

    let profile = if cfg!(debug_assertions) {
        "debug".to_string()
    } else {
        "release".to_string()
    };

    let out = Results {
        engine: "tree-walker (sui_eval::eval, --no-vm equivalent)".to_string(),
        nix_version: nix_version(),
        sui_version: env!("CARGO_PKG_VERSION").to_string(),
        profile,
        spawn_floor_us,
        shapes: results,
        summary,
        correctness,
    };

    // Serialize typed → JSON (serde, NOT format!()).
    let target = target_dir();
    let json_path = target.join("vs-nix-hotshapes.results.json");
    let json = serde_json::to_string_pretty(&out).expect("serialize results.json");
    std::fs::write(&json_path, &json).expect("write results.json");

    let md = render_md(&out);
    let md_path = target.join("vs-nix-hotshapes.md");
    std::fs::write(&md_path, &md).expect("write .md mirror");

    eprintln!("\nwrote {}", json_path.display());
    eprintln!("wrote {}", md_path.display());
    eprintln!(
        "\nengine geomean (comparable shapes): {:.2}× over {} shapes",
        out.summary.engine_geomean, out.summary.n_engine_comparable
    );
    eprintln!("wall geomean (all shapes):          {:.1}×", out.summary.wall_geomean);
    eprintln!("drvPath correctness rows:           {}", out.correctness.len());
}
