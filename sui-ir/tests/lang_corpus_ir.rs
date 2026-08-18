//! The nix-language corpus, run against `eval_ir` and byte-compared with the
//! tree-walker.
//!
//! # Why this file exists
//!
//! `sui-eval/tests/fixtures/lang/` holds 117 fixtures vendored from CppNix's
//! own `tests/functional/lang/`. Until now **exactly one call site read them**
//! — `sui-eval/tests/lang_corpus.rs`, which drives the tree-walker. So the
//! corpus measured one of sui's three engines, and the other two were covered
//! by whatever hand-written rows someone happened to write.
//!
//! That is the shape behind every divergence found in this push: the walker
//! and the VM disagreed on the `builtins` name set, on `builtins.nixVersion`,
//! on filesystem reads, and on whether a `with` could shadow `break` — each
//! discovered by accident rather than by a gate, because no shared corpus ran
//! on more than one engine.
//!
//! # What is compared
//!
//! Not JSON — the **format-locked render pair**
//! (`sui_eval::render::render_tree` / `sui_ir::render::render_ir_value`),
//! which is the existing mechanism `sui-ir/tests/file_differential.rs` already
//! uses. It deep-forces and propagates errors, so a thunk placeholder cannot
//! silently compare equal to a value. Comparing `--json` here would be strictly
//! worse: both engines render an unforceable value as a *placeholder string*,
//! and two placeholders compare equal.
//!
//! # The allowlist shrinks, never grows
//!
//! `KNOWN_GAPS` follows the model of `eval_differential.rs`'s
//! `SUPPLEMENT_KNOWN_GAPS`: a fixture may sit here only with a typed reason,
//! and the count is pinned so adding one is a deliberate, reviewed edit. A gap
//! that closes must be REMOVED from the list — the test fails if an entry
//! starts passing, because an allowlist nobody prunes silently becomes the
//! coverage number.

use std::path::{Path, PathBuf};

mod common;
use common::render::{render_ir_value, render_tree};

use sui_eval::Evaluator as _;
use sui_ir::eval_ir_file;
use sui_ir::file_eval::clear_file_caches;

/// Fixtures `eval_ir` does not yet match, each with a typed reason.
///
/// **This list may only shrink.** An entry that starts passing fails the run,
/// so a closed gap must be pruned in the same commit — an allowlist nobody
/// prunes quietly becomes the coverage number.
///
/// Measured on first run: **98/117 agree, 19 diverge.** They are NOT one
/// severity, and the grouping is the useful part:
///
/// - **DECLARED (12)** — `eval_ir` returns a typed
///   `IrEvalError::Unsupported`. It is explicitly a *pure-subset* evaluator
///   and these constructs are outside the subset by design. A loud refusal, not
///   a wrong answer. This is the good pattern the other engines lack.
/// - **UNDECLARED ERROR (6)** — `eval_ir` fails where the walker succeeds, but
///   not through the `Unsupported` channel. Each is a real gap; the error text
///   is recorded so a fix can be matched to it.
/// - **WRONG VALUE (1)** — the serious one. See `merge-dynamic-attrs`.
const KNOWN_GAPS: &[(&str, &str)] = &[
    // ── DECLARED: typed Unsupported, outside the pure subset by design ──
    // `builtins.toFile` is store-effecting — it computes a content-addressed
    // store path and writes the object — so it is outside the pure subset by
    // construction, not a missing implementation. eval_ir REFUSES it with a
    // typed `Unsupported` ("builtin not implemented by the pure-subset IR
    // evaluator: toFile") rather than computing a path, which is the honest
    // failure mode: contrast the bytecode VM, which computes a WRONG path for
    // the same fixture because it silently drops string context.
    ("eval-okay-tofile-refs", "unsupported:store-effecting-builtin"),
    // ★ The tree-walker adopted `sui-normalize`'s parse-time splice on
    // 2026-08-18 and this fixture graduated out of quarantine; eval_ir has
    // not been wired yet, so it is now the engine that is behind.
    //
    //     { a = rec { b = c + 1; d = 2; }; a.c = d + 3; }.a.b
    //     walker 6 (= nix)   eval_ir  undefined variable 'c'
    //
    // The error states the defect exactly: `c` exists only AFTER the dotted
    // binding `a.c` is spliced INTO the `rec` literal, so an engine that
    // never splices cannot resolve it. Not an eval_ir regression — the walker
    // moved. Closes when eval_ir adopts the plan.
    ("eval-okay-regrettable-rec-attrset-merge", "unsupported:no-attrset-splice"),
    ("eval-okay-attrs", "unsupported:legacy-let"),
    ("eval-okay-attrs2", "unsupported:construct"),
    ("eval-okay-flatten", "unsupported:construct"),
    ("eval-okay-let", "unsupported:legacy-let"),
    ("eval-okay-list", "unsupported:construct"),
    ("eval-okay-remove", "unsupported:construct"),
    ("eval-okay-scope-4", "unsupported:construct"),
    ("eval-okay-scope-6", "unsupported:construct"),
    ("eval-okay-with", "unsupported:construct"),
    ("eval-okay-break", "unsupported:builtin:break"),
    ("eval-okay-flake-ref-to-string", "unsupported:builtin:flakeRefToString"),
    ("eval-okay-parse-flake-ref", "unsupported:builtin:parseFlakeRef"),
    // ── UNDECLARED ERROR: eval_ir fails where the walker succeeds ──
    ("eval-okay-baseNameOf", "err:assertion-failed"),
    ("eval-okay-delayed-with-inherit", "err:undefined-variable-b (delayed `with` + inherit scoping)"),
    ("eval-okay-foldlStrict-lazy-initial-accumulator", "err:forces-an-accumulator-nix-never-forces"),
    ("eval-okay-getattrpos", "err:expected-set-got-null (unsafeGetAttrPos returns null)"),
    ("eval-okay-sort", "err:lessThan-expected-comparable-types"),
    ("eval-okay-substring-context", "err:empty-substring-drops-string-context"),
    // ── WRONG VALUE — the one that is a silent divergence, not a refusal ──
    //
    // `{ set1 = { a = 1; }; set1 = { "${"b"+""}" = 2; }; }` must merge to
    // `{a=1; b=2;}`; eval_ir yields `{b=2;}` — it DROPS the statically-keyed
    // half when a brace-merge combines a static attrset with a dynamically-keyed
    // one. Verified against the vendored CppNix `.exp`, which expects all four
    // sets to be `{a:1,b:2}`, and the tree-walker matches it.
    //
    // Note `set3`/`set4` in the same fixture use the DOTTED form and are
    // correct, so the bug is specific to brace-merge, which narrows the fix.
    //
    // This is the only row here that produces a plausible wrong ANSWER rather
    // than an error, and is therefore the one to close first.
    ("eval-okay-merge-dynamic-attrs", "WRONG-VALUE:brace-merge-drops-static-half"),
];

fn lang_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // sui-ir/ -> workspace root
    p.push("sui-eval/tests/fixtures/lang");
    p
}

/// The active corpus — `eval-okay-*.nix` directly under `lang/`, matching the
/// walker's own non-recursive discovery so both engines see the same set.
fn fixtures() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(lang_dir()) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("nix") {
            continue;
        }
        if !p
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("eval-okay-"))
        {
            continue;
        }
        out.push(p);
    }
    out.sort();
    out
}

fn stem(p: &Path) -> String {
    p.file_stem().unwrap_or_default().to_string_lossy().to_string()
}

fn gap_reason(name: &str) -> Option<&'static str> {
    KNOWN_GAPS.iter().find(|(n, _)| *n == name).map(|(_, r)| *r)
}

fn tree_outcome(p: &Path) -> Result<String, String> {
    sui_eval::builtins::clear_import_cache();
    let v = sui_eval::TreeWalkEvaluator
        .eval_file(p)
        .map_err(|e| e.to_string())?;
    render_tree(&v)
}

fn ir_outcome(p: &Path) -> Result<String, String> {
    clear_file_caches();
    eval_ir_file(p)
        .map_err(|e| e.to_string())
        .and_then(|v| render_ir_value(&v).map_err(|e| e.to_string()))
}

/// Every active fixture renders identically on `eval_ir` and the tree-walker.
///
/// Both-error counts as agreement — two engines refusing the same program for
/// the same reason is parity, and demanding a value would bias the corpus
/// toward whatever both happen to implement.
#[test]
fn eval_ir_matches_the_tree_walker_on_the_lang_corpus() {
    // Run on a big-stack worker, the way the CLI does (`main.rs` spawns 256 MB
    // for eval). Several corpus fixtures recurse deeply enough to blow a test
    // thread's default 2 MB stack — measured: this overflows with SIGABRT
    // before reaching a single assertion. The walker's own `lang_corpus.rs`
    // escapes it by comparing shallow JSON; this file deep-forces through the
    // render pair, which is stricter and therefore hits real depth.
    std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(corpus_body)
        .expect("spawn corpus worker")
        .join()
        .expect("corpus worker panicked");
}

fn corpus_body() {
    let fixtures = fixtures();

    // ANTI-VACUITY, before the verdict. An empty or truncated discovery would
    // otherwise report "0 mismatches" over nothing at all — the exact failure
    // this corpus is being extended to prevent.
    assert!(
        fixtures.len() > 100,
        "found only {} lang fixtures — discovery is broken, and a clean result \
         over a broken scan is not a clean result",
        fixtures.len()
    );

    let mut matched_value = 0usize;
    let mut matched_both_error = 0usize;
    let mut gaps = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for path in &fixtures {
        let name = stem(path);
        let tw = tree_outcome(path);
        let ir = ir_outcome(path);
        let agree = match (&tw, &ir) {
            (Ok(a), Ok(b)) => a == b,
            (Err(_), Err(_)) => true,
            _ => false,
        };

        match (gap_reason(&name), agree) {
            // Allowlisted and still diverging — expected.
            (Some(_), false) => gaps += 1,
            // Allowlisted but now agreeing — the gap closed; prune the entry.
            (Some(reason), true) => failures.push(format!(
                "  {name}: listed in KNOWN_GAPS ({reason}) but now AGREES — \
                 remove the entry. An allowlist nobody prunes becomes the \
                 coverage number."
            )),
            (None, true) => {
                if tw.is_ok() {
                    matched_value += 1;
                } else {
                    matched_both_error += 1;
                }
            }
            (None, false) => failures.push(format!(
                "  {name}:\n      walker: {}\n      eval_ir: {}",
                tw.as_ref().map_or_else(|e| format!("ERR {e}"), Clone::clone),
                ir.as_ref().map_or_else(|e| format!("ERR {e}"), Clone::clone),
            )),
        }
    }

    eprintln!(
        "eval_ir vs tree-walker on lang corpus: {}/{} agree ({} value, {} both-error), {} known gaps",
        matched_value + matched_both_error,
        fixtures.len(),
        matched_value,
        matched_both_error,
        gaps
    );

    // A run where EVERY row was a both-error agreement would report full
    // agreement while proving only that two engines fail together. Require
    // that most rows actually produced a value.
    assert!(
        matched_value > fixtures.len() / 2,
        "only {matched_value} of {} rows agreed on a VALUE; the rest agreed by \
         both failing. Two engines erroring in unison is not parity evidence.",
        fixtures.len()
    );

    assert!(
        failures.is_empty(),
        "\n{} of {} lang fixtures diverge between eval_ir and the tree-walker:\n{}",
        failures.len(),
        fixtures.len(),
        failures.join("\n")
    );
}

/// Every allowlist entry must name a fixture that is actually in the corpus.
///
/// Found by a red-run that failed to go red: an entry naming a fixture the
/// scan never sees is silently inert, so a typo — or a name that later moves
/// into `known_broken/` — would sit here forever looking like it suppresses
/// something. An allowlist entry that suppresses nothing is indistinguishable
/// from one that suppresses a real gap, which is the whole failure mode this
/// file exists to remove.
#[test]
fn every_known_gap_names_a_real_fixture() {
    let present: Vec<String> = fixtures().iter().map(|p| stem(p)).collect();
    let phantom: Vec<&str> = KNOWN_GAPS
        .iter()
        .map(|(n, _)| *n)
        .filter(|n| !present.iter().any(|p| p == n))
        .collect();
    assert!(
        phantom.is_empty(),
        "these KNOWN_GAPS entries name no active fixture: {phantom:?}. \
         Either the name is a typo, or the fixture moved to known_broken/ — \
         in which case delete the entry, because it is suppressing nothing."
    );
}

/// The allowlist is pinned, so growing it is a deliberate edit.
#[test]
fn the_known_gap_list_is_pinned() {
    assert_eq!(
        KNOWN_GAPS.len(),
        21,
        "KNOWN_GAPS changed size. It may SHRINK freely — delete the entry and \
         update this number. Growing it means eval_ir regressed against the \
         corpus, or a newly-vendored fixture was allowlisted instead of fixed; \
         either way say which in the commit."
    );
}
