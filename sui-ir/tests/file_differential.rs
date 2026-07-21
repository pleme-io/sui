//! L3 slice 3 — the FILE differential: multi-file fixtures under
//! `tests/fixtures/` evaluated end-to-end through BOTH engines — the
//! tree-walker's `TreeWalkEvaluator::eval_file` (the semantic oracle) vs
//! the IR engine's `eval_ir_file` (lower-once program cache + `import`) —
//! and byte-compared through the shared normalized render.
//!
//! Also proves, mechanically:
//! * **lower-once** — the program cache holds exactly one entry per file,
//!   and a full re-evaluation (value cache cleared) does not grow it;
//! * **import identity** — `import ./a.nix == import ./a.nix` is `true`
//!   (the value cache returns the same value, like the walker's);
//! * **typed circular imports** — `cycle_a.nix ↔ cycle_b.nix` is a typed
//!   [`IrEvalError::ImportCycle`] on the IR engine (IR-only test: the
//!   walker, like CppNix, would recurse until the stack dies);
//! * the **file A/B** (`--ignored --nocapture`): the same fixture through
//!   walker-per-eval vs lower-once + `eval_ir`, cold and warm.

use std::path::PathBuf;

use sui_eval::Evaluator;
use sui_ir::eval_ir::IrEvalError;
use sui_ir::{clear_file_caches, clear_import_value_cache, eval_ir_file, program_cache_len};

mod common;
use common::render::{render_ir_value, render_tree};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn tree_file_outcome(path: &PathBuf) -> Result<String, String> {
    sui_eval::builtins::clear_import_cache();
    let value = sui_eval::TreeWalkEvaluator
        .eval_file(path)
        .map_err(|e| e.to_string())?;
    render_tree(&value)
}

fn ir_file_outcome(path: &PathBuf) -> Result<String, IrEvalError> {
    clear_file_caches();
    eval_ir_file(path).and_then(|v| render_ir_value(&v))
}

// ── 1. the file corpus ────────────────────────────────────────────────────

/// Every fixture root byte-matches across the two engines: a let-heavy lib
/// file, an importing file (file + directory + recursive imports, relative
/// path literals, import-identity equality), a directory `default.nix`, an
/// attrset-module shape, and the A/B fixture.
#[test]
fn file_corpus_differential() {
    let roots = [
        "lib.nix",
        "importer.nix",
        "dir/default.nix",
        "module.nix",
        "ab_file.nix",
    ];
    let mut failures = Vec::new();
    for name in roots {
        let path = fixture(name);
        let tree = tree_file_outcome(&path);
        let ir = ir_file_outcome(&path);
        match (tree, ir) {
            (Ok(t), Ok(i)) if t == i => {}
            (Ok(t), Ok(i)) => failures.push(format!(
                "fixture {name} DIVERGED\n  tree: {t}\n  ir:   {i}"
            )),
            (Err(te), Ok(i)) => failures.push(format!(
                "fixture {name}: tree-walker errors ({te}) but IR evaluates to {i}"
            )),
            (Ok(t), Err(ie)) => failures.push(format!(
                "fixture {name}: tree-walker yields {t} but IR errors ({ie})"
            )),
            (Err(te), Err(ie)) => failures.push(format!(
                "fixture {name}: BOTH engines error (fixtures must evaluate) tree={te} ir={ie}"
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "{} file-corpus fixtures failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

// ── 2. lower-once (the program cache proof) ───────────────────────────────

/// `importer.nix` pulls 4 more files (lib, dir/default, dir/data, module).
/// After one evaluation the program cache holds exactly 5 programs; a FULL
/// re-evaluation (import value cache cleared, so every file's code re-runs)
/// does not grow it — parse + lower happened once per file.
#[test]
fn lower_once_program_cache() {
    clear_file_caches();
    let path = fixture("importer.nix");
    let first = eval_ir_file(&path)
        .and_then(|v| render_ir_value(&v))
        .expect("importer evaluates");
    assert_eq!(program_cache_len(), 5, "importer + 4 imported files");
    clear_import_value_cache();
    let second = eval_ir_file(&path)
        .and_then(|v| render_ir_value(&v))
        .expect("importer re-evaluates");
    assert_eq!(
        program_cache_len(),
        5,
        "re-evaluation must not re-lower any file"
    );
    assert_eq!(first, second, "re-evaluation is deterministic");
}

// ── 3. typed circular-import detection (IR-only) ──────────────────────────

/// The walker (like CppNix) has no cycle detection — running it on this
/// fixture would recurse until the stack dies, so this is deliberately an
/// IR-only assertion of the TYPED error class.
#[test]
fn circular_import_is_a_typed_error() {
    clear_file_caches();
    let result = eval_ir_file(&fixture("cycle_a.nix"));
    match result {
        Err(IrEvalError::ImportCycle(chain)) => {
            assert!(
                chain.contains("cycle_a.nix") && chain.contains("cycle_b.nix"),
                "cycle chain names both files: {chain}"
            );
        }
        other => panic!("expected a typed ImportCycle, got {other:?}"),
    }
}

// ── 4. import edge semantics ──────────────────────────────────────────────

/// Directory import resolves to `default.nix` (the walker's
/// `resolve_import` rule, mirrored) — `import ./dir` inside `importer.nix`
/// already exercises this end-to-end; here the DIRECTORY ITSELF is the
/// import target from a scratch expression file.
#[test]
fn directory_import_resolves_default_nix() {
    clear_file_caches();
    let dir = fixture("dir");
    // Import via the public entry: evaluate a scratch file that imports
    // the directory. Write it to a temp dir so the fixture tree stays
    // pristine.
    let tmp = std::env::temp_dir().join("sui-ir-slice3-dirimport");
    std::fs::create_dir_all(&tmp).expect("temp dir");
    let scratch = tmp.join("scratch.nix");
    let mut src = String::from("(import ");
    src.push_str(&dir.display().to_string());
    src.push_str(").marker");
    std::fs::write(&scratch, &src).expect("write scratch");
    let ir = eval_ir_file(&scratch)
        .and_then(|v| render_ir_value(&v))
        .expect("directory import evaluates");
    assert_eq!(ir, "\"dir-default\"");
    // Oracle agreement on the same scratch file.
    let tree = tree_file_outcome(&scratch).expect("walker evaluates scratch");
    assert_eq!(tree, ir);
}

// ── 5. the file A/B (run with --ignored --nocapture) ──────────────────────

/// Walker-per-eval vs lower-once + `eval_ir` over `ab_file.nix` (which
/// imports lib.nix + dir). Three timed columns per round:
/// * `tree` — the walker's `eval_file` (re-reads + re-parses every file,
///   import value cache cleared per eval);
/// * `ir-cold` — `eval_ir_file` with ALL caches cleared per eval (pays
///   parse + lower every time: the honest first-eval cost);
/// * `ir-warm` — `eval_ir_file` with only the import VALUE cache cleared
///   per eval (every file's code re-runs, but parse + lower happened once:
///   the lower-once destination).
#[test]
#[ignore = "file A/B timing — run explicitly with --ignored --nocapture"]
fn file_ab_ir_vs_tree_walker() {
    use std::time::Instant;

    let path = fixture("ab_file.nix");

    // Byte-agreement first — a timing of diverging engines is meaningless.
    let tree_txt = tree_file_outcome(&path).expect("walker evaluates ab_file");
    let ir_txt = ir_file_outcome(&path).expect("ir evaluates ab_file");
    assert_eq!(tree_txt, ir_txt, "A/B fixture must agree before timing");

    const ITERS: u32 = 300;
    const ROUNDS: usize = 5;
    println!(
        "file A/B — {ITERS} evals/round, {ROUNDS} interleaved rounds, result {tree_txt}"
    );
    println!("round | tree-walker | ir-cold | ir-warm | tree/cold | tree/warm");
    for round in 0..ROUNDS {
        let t0 = Instant::now();
        for _ in 0..ITERS {
            sui_eval::builtins::clear_import_cache();
            let v = sui_eval::TreeWalkEvaluator
                .eval_file(&path)
                .expect("tree evals");
            std::hint::black_box(sui_eval::eval::force_value(&v).expect("forces"));
        }
        let tree_dt = t0.elapsed();

        let t1 = Instant::now();
        for _ in 0..ITERS {
            clear_file_caches();
            std::hint::black_box(eval_ir_file(&path).expect("ir cold evals"));
        }
        let cold_dt = t1.elapsed();

        // Prime the program cache, then measure warm evals.
        clear_file_caches();
        std::hint::black_box(eval_ir_file(&path).expect("ir warm prime"));
        let t2 = Instant::now();
        for _ in 0..ITERS {
            clear_import_value_cache();
            std::hint::black_box(eval_ir_file(&path).expect("ir warm evals"));
        }
        let warm_dt = t2.elapsed();

        println!(
            "{round:>5} | {tree_us:>9}us | {cold_us:>7}us | {warm_us:>7}us | {rc:>8.2}x | {rw:>8.2}x",
            tree_us = tree_dt.as_micros(),
            cold_us = cold_dt.as_micros(),
            warm_us = warm_dt.as_micros(),
            rc = tree_dt.as_secs_f64() / cold_dt.as_secs_f64(),
            rw = tree_dt.as_secs_f64() / warm_dt.as_secs_f64(),
        );
    }
}
