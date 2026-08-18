//! The nix-language corpus, run against the **bytecode VM** under
//! `SUI_VM_STRICT=1`, byte-compared with the tree-walker.
//!
//! # Why this file exists
//!
//! `sui-eval/tests/fixtures/lang/` holds 117 fixtures vendored from CppNix's
//! own `tests/functional/lang/`. Until today they had exactly **one** consumer
//! — `sui-eval/tests/lang_corpus.rs`, driving the tree-walker. A second landed
//! for `eval_ir` (`sui-ir/tests/lang_corpus_ir.rs`) and found an infinite
//! recursion and a wrong value on its first run. This is the third engine's.
//!
//! # Three things this harness had to get right, or it would have lied
//!
//! **1. `SUI_VM_STRICT=1`, asserted rather than assumed.** The VM delegates to
//! the tree-walker at three granularities (`sui-bytecode/src/fallback.rs`).
//! Without the latch, a fixture the VM cannot handle is silently answered by
//! the walker and the run reports coverage it does not have — measured
//! elsewhere in this repo as *a VM failing 100% of an expression set passing
//! 36/36*. The latch is process-latched via `OnceLock`, so this file sets it
//! before the first evaluation **and then asserts it took**; a run that could
//! not arm it goes red instead of reporting an inflated number.
//!
//! `Layer::Builtin` is counted, never fatal — bridging a builtin the VM has no
//! native implementation for is its architecture, not a failure. So agreement
//! is reported in two figures: how many fixtures agreed, and how many of those
//! needed at least one walker builtin. The second is the honest discount on the
//! first.
//!
//! **2. Placeholders must not compare equal.** Both engines can render an
//! unforceable value as a placeholder *string*, and two placeholders are `==`.
//! Three guards:
//!
//! - `sui_bytecode::render::render_vm` returns `Err` on a residual thunk
//!   instead of the `<<lambda>>` that `to_string_keyed` would produce or the
//!   `"<thunk>"` the binary's private `string_keyed_to_json` would produce.
//! - `sui_eval::render::render_tree` deep-forces and propagates force errors,
//!   so the walker cannot answer with a placeholder either.
//! - [`no_agreement_rests_on_a_placeholder`] re-renders every **agreeing** row
//!   and fails if any contains `<<lambda>>`, `<<builtin `, or the depth
//!   sentinel `<...>` — the residual case where two genuine functions render
//!   identically without their bodies having been compared. Measured: zero
//!   rows. That assertion carries its own denominator, so a scan that stopped
//!   finding rows fails rather than passes.
//!
//! **3. Attrsets must sort by name.** The VM keys attrsets by `Symbol`, whose
//! `Ord` is interning order. Comparing that against the walker's name-ordered
//! render would have reported a divergence on nearly every multi-key fixture,
//! all of it spurious. `render_vm` re-keys by resolved name first.
//!
//! # What is compared
//!
//! Not JSON — the **format-locked normalized render**, the mechanism
//! `sui-ir`'s differentials already use. Comparing `--json` would be strictly
//! worse for the placeholder reason above, and worse again here because the
//! two JSON paths in this workspace do not agree with each other (see
//! `render.rs`'s module docs).
//!
//! # The allowlist shrinks, never grows
//!
//! `KNOWN_GAPS` carries a typed reason per entry, the size is pinned, an entry
//! that starts passing fails the run, and an entry naming a fixture that does
//! not exist fails the run.

use std::path::{Path, PathBuf};
use std::sync::Once;

use sui_bytecode::fallback::{self, Layer};
use sui_bytecode::render::render_vm;
use sui_bytecode::{EvalError, FileEvalError};
use sui_eval::Evaluator as _;
use sui_eval::render::render_tree;

/// Fixtures the VM does not match, each with a typed reason.
///
/// **This list may only shrink.** An entry that starts passing fails the run,
/// so a closed gap must be pruned in the same commit — an allowlist nobody
/// prunes quietly becomes the coverage number.
///
/// Measured on first run, 2026-08-17, `SUI_VM_STRICT=1`:
/// **77/117 agree, 40 diverge** — and the 40 are not one severity:
///
/// - **DECLARED (5)** — the compiler refuses in its own words
///   (`unsupported expression: …`). A loud refusal, not a wrong answer. Note
///   this is the *only* declared channel that fired: `imported-file=0` and
///   `whole-expression=0`, i.e. the strict latch never had an occasion to
///   refuse on this corpus. Strict is still armed and asserted so it cannot
///   silently un-arm later, but it did not change this measurement.
/// - **UNDECLARED ERROR (30)** — the VM fails where the walker succeeds, and
///   **14 of the 30 are ONE root**, isolated by probe rather than inferred:
///   the VM loses the `with` scope across an `import`. `with builtins; rec { f
///   = l: head l; }` evaluates correctly INLINE (3 shapes checked) and fails
///   with `undefined variable: head` when the identical source is imported and
///   `f` applied. `lib.nix` — which 14 corpus fixtures open with — is exactly
///   that shape, and it imports *fine*: it renders as an attrset of lambdas.
///   The failure is on APPLICATION, so the closure is not capturing its
///   definition-site with-scope. Those 14 carry `with-scope-lost-across-import`.
/// - **WRONG VALUE (5)** — the serious ones: a plausible wrong answer with no
///   error. All five verified against the vendored CppNix `.exp`, which the
///   tree-walker matches and the VM does not. Detailed individually below.
///
/// There were **no** `VM-ONLY-SUCCESS` rows and **no** both-error agreements:
/// all 77 agreements are on a value, and
/// [`no_agreement_rests_on_a_placeholder`] confirms none of them is a
/// placeholder.
///
/// Fallback counters for the run: `builtin=4 imported-file=0
/// whole-expression=0`. Read those honestly — **4** walker-builtin crossings
/// across the whole corpus, and **1** of the 77 agreements involved one, so
/// the discount on "the VM computed this" is one fixture, not seventy-seven.
/// And no residual thunk reached the renderer: zero rows failed with
/// `unforced thunk`, so the placeholder-refusal arm never had to fire here —
/// it is proven by `render.rs`'s unit test, not by this corpus.
const KNOWN_GAPS: &[(&str, &str)] = &[
    // ── WRONG VALUE (5) — a plausible wrong answer, no error raised. ────────
    //
    // These are the finding. Each is adjudicated by the vendored CppNix `.exp`,
    // not by the tree-walker, so neither party to the disagreement is judging
    // its own case.
    //
    // `derivation { name="a"; builder="/foo"; system="i686-linux"; }` hashes to
    // a DIFFERENT store path on the VM than on the walker. `.exp` pins
    // `…-mzgwvrjjir216ra58mwwizi8wj6y9ddr-…`; the walker matches it, the VM
    // produces `…-ijbh4nljcasfap54cxnplbrv6cbnw7vw-…`. A wrong drvPath is the
    // worst shape of wrong: it is a legal-looking store path that names
    // something that was never built from this expression.
    ("eval-okay-derivation-legacy", "WRONG-VALUE:drv-hash-differs-from-cppnix-exp"),
    // `drvA1 == drvA1 // { dummy = 1; }` must be `true`: CppNix compares two
    // derivations by `outPath` alone, so an extra attribute cannot make them
    // unequal. `.exp` is `[true,true,true,false]`; the VM answers
    // `[true,true,false,false]` — it is comparing derivations structurally.
    ("eval-okay-eq-derivations", "WRONG-VALUE:derivation-equality-not-by-outPath"),
    // `let f = x: x; in { a = f; } == { a = f; }` must be `true` — CppNix's
    // documented value-identity optimization: the same function value compared
    // with itself short-circuits to equal. The VM answers `false`, i.e. it has
    // no pointer-identity arm and falls through to "functions are never equal".
    ("eval-okay-equal-function-attrset-identical", "WRONG-VALUE:no-value-identity-optimization"),
    // Same defect, list-shaped: `[ f ] == [ f ]`.
    ("eval-okay-equal-function-list-identical", "WRONG-VALUE:no-value-identity-optimization"),
    // Same defect again, and the widest instance of it: 14 rows covering every
    // shape in which a shared function reaches a nested comparison (list,
    // attrs, `inherit`, nested path, `with`-scope, `builtins.elem`,
    // `builtins.filter`, a merged attrset literal). nix says `true` to all 14
    // — its pointer hack fires because a nested element really is one `Value*`
    // on both sides — and the walker matches. The VM answers `false` to ALL
    // 14, uniformly, because `deep_eq` forces at every level and has no
    // identity arm at all.
    //
    // Landed 2026-08-18 as the CALIBRATION for the walker's `eq_operator`
    // split (`f == f` false at the operator, true nested). It is listed here
    // rather than weakened: the VM needs the OPPOSITE addition to the walker's
    // — a pre-force identity arm, not an entry-point split — and a fixture
    // that both engines can satisfy today would not have caught the walker bug
    // it was written for.
    ("eval-okay-equal-function-alias-nested", "WRONG-VALUE:no-value-identity-optimization"),
    // `builtins.toXML` on any function. nix writes
    // `<function><varpat name="x" /></function>` (and `<attrspat>` with
    // `ellipsis="1"` / `name="args"` for a pattern); the walker and the IR now
    // match it byte-for-byte. The VM renders `<null />`.
    //
    // Root is the BRIDGE CROSSING, not toXML: `toXML` is bridge-dispatched
    // (`vm.rs`), the value is flattened through `to_string_keyed`, and
    // `StringKeyedValue::Lambda` maps to `Value::Null` in
    // `sui-eval/src/convert.rs` — "bare lambdas cannot cross the boundary".
    // By then the parameter names are already gone, so this cannot be fixed in
    // the renderer; the crossing has to stop discarding the lambda. Recorded
    // rather than papered over, because a function rendered as `null` is a
    // silent wrong value at a GENERAL crossing, not a toXML quirk.
    ("eval-okay-toxml-functions", "WRONG-VALUE:bridge-crossing-maps-lambda-to-null"),
    // ★ THE VM'S WORST DEFECT, and until 2026-08-18 NO fixture exercised it.
    //
    // `VMValue::String` carries no string context ("context tracking deferred
    // to Phase 2"), so a string that interpolates a store path forgets that it
    // did. `builtins.toFile` hashes its content's REFERENCE SET into the store
    // path, so the VM computes a different — and wrong — path for every
    // toFile whose content interpolates anything.
    //
    // The rows say it precisely: `none` (no context) AGREES, and every
    // reference-carrying row differs. The VM's `oneRef` is
    // `vfzawb40l19d2i4n49grnfm3wrkiq63g`, which is exactly the value the
    // TREE-WALKER produced before its own context bug was fixed the same day —
    // i.e. the VM is reproducing the context-less hash, which is the defect
    // stated as a byte.
    //
    // This is the same root that makes every VM-computed drvPath wrong for a
    // derivation that interpolates another (`inputDrvs`/`inputSrcs` come from
    // string context). It is why `--vm` is no longer the default engine. The
    // corpus was blind to it until this fixture landed; it is listed here so
    // the blindness is recorded rather than restored.
    ("eval-okay-tofile-refs", "WRONG-VALUE:vm-has-no-string-context"),
    // The VM cannot serialize an ATTRSET to JSON at all — `toJSON` on a list
    // or a scalar works natively, but an attrset raises
    // `toJSON: attrset conversion requires interner` and falls through to the
    // whole-expression boundary, which strict mode then refuses.
    //
    // Pre-existing and verified as such by re-running the probe with the
    // day's JSON-formatter change stashed: the same error. It is an
    // UNDECLARED-ERROR, not a WRONG-VALUE, which is the honest failure mode —
    // the VM says it cannot rather than answering wrongly. Listed so the
    // distinction is on the record and the fixture keeps its coverage of the
    // walker and IR, which both match nix byte-for-byte here.
    ("eval-okay-tojson-floats", "UNDECLARED-ERROR:vm-tojson-attrset-needs-interner"),
    // ★ The tree-walker adopted `sui-normalize`'s parse-time splice on
    // 2026-08-18 and this fixture graduated out of quarantine; the VM has not
    // been wired yet, so it is now the engine that is behind.
    //
    //     { a = rec { b = c + 1; d = 2; }; a.c = d + 3; }.a.b
    //     walker 6 (= nix)   vm  compile error: unresolved variable: c
    //
    // The VM's error is the defect stated precisely: `c` exists only AFTER
    // the dotted binding `a.c` is spliced INTO the `rec` literal, so an
    // engine that never splices cannot resolve it. Not a VM regression — the
    // walker moved. Closes when the VM adopts the plan.
    ("eval-okay-regrettable-rec-attrset-merge", "declared:vm-has-no-attrset-splice"),
    // The most dangerous of the five. `builtins.tryEval (assert false; "y")`
    // must be `{ success = false; value = false; }`. The VM returns the string
    // `"y"` — the body's value, UNWRAPPED, with the failing assertion never
    // forced. Two defects in one: `tryEval` does not catch `assert` (it does
    // catch `throw` — the sibling `z` row is correct), and on that path it
    // returns the bare body instead of the `{success, value}` attrset. A caller
    // reading `.success` gets an attribute error; a caller using the value gets
    // a silently wrong one from an assertion that was supposed to have failed.
    ("eval-okay-tryeval", "WRONG-VALUE:tryEval-misses-assert-and-returns-unwrapped-body"),
    // ── DECLARED (5) — `CompileError::Unsupported`, the VM saying so. ───────
    ("eval-okay-dynamic-attrs", "declared:unsupported-interpolated-string-attr-keys"),
    ("eval-okay-dynamic-attrs-2", "declared:unsupported-interpolated-string-attr-keys"),
    ("eval-okay-dynamic-attrs-3", "declared:unsupported-dynamic-attr-keys"),
    ("eval-okay-dynamic-attrs-bare", "declared:unsupported-interpolated-string-attr-keys"),
    ("eval-okay-merge-dynamic-attrs", "declared:unsupported-interpolated-string-attr-keys"),
    // ── UNDECLARED ERROR, root A (14) — one bug, fourteen fixtures. ─────────
    //
    // Every one of these opens `with import ./lib.nix;`, and `lib.nix` opens
    // `with builtins;`. The name in each reason is the one that came back
    // undefined — all of them names `lib.nix` resolves through its OWN
    // top-level `with`. Probe (3 inline shapes pass, the imported shape fails,
    // `lib.nix` itself imports fine and only fails on application) puts the bug
    // in with-scope capture by a closure crossing `import`, NOT in `import` and
    // NOT in `with`. Closing it should close all fourteen at once.
    ("eval-okay-attrnames", "with-scope-lost-across-import:tail"),
    ("eval-okay-attrs5", "with-scope-lost-across-import:head"),
    ("eval-okay-closure", "with-scope-lost-across-import:lessThan"),
    ("eval-okay-concatmap", "with-scope-lost-across-import:genList"),
    ("eval-okay-elem", "with-scope-lost-across-import:genList"),
    ("eval-okay-filter", "with-scope-lost-across-import:genList"),
    ("eval-okay-flatten", "with-scope-lost-across-import:isList"),
    ("eval-okay-foldlStrict", "with-scope-lost-across-import:genList"),
    ("eval-okay-groupBy", "with-scope-lost-across-import:genList"),
    ("eval-okay-list", "with-scope-lost-across-import:tail"),
    ("eval-okay-listtoattrs", "with-scope-lost-across-import:tail"),
    ("eval-okay-map", "with-scope-lost-across-import:tail"),
    ("eval-okay-partition", "with-scope-lost-across-import:genList"),
    ("eval-okay-zipAttrsWith", "with-scope-lost-across-import:genList"),
    // ── UNDECLARED ERROR, other roots (16) ─────────────────────────────────
    //
    // Error text recorded verbatim so a fix can be matched to a row without
    // re-running the corpus to find out what it said.
    ("eval-okay-baseNameOf", "err:assertion-failed"),
    ("eval-okay-callable-attrs", "err:not-a-function-set (no __functor dispatch)"),
    ("eval-okay-context", "err:abort-context-not-discarded (no string context)"),
    ("eval-okay-delayed-with", "err:derivation-attr-system-expected-string-got-thunk"),
    ("eval-okay-delayed-with-inherit", "err:undefined-variable-b (delayed `with` + inherit)"),
    ("eval-okay-foldlStrict-lazy-elements", "err:forces-an-element-nix-never-forces"),
    ("eval-okay-foldlStrict-lazy-initial-accumulator", "err:forces-an-accumulator-nix-never-forces"),
    ("eval-okay-functionargs", "err:assertion-failed"),
    ("eval-okay-getattrpos", "err:expected-set-got-null (unsafeGetAttrPos returns null)"),
    ("eval-okay-intersectAttrs", "err:throw-f (forces a value nix leaves lazy)"),
    ("eval-okay-json-roundtrip", "err:toJSON-attrset-conversion-requires-interner"),
    ("eval-okay-null-dynamic-attrs", "err:attrset-key-expected-string-got-null"),
    ("eval-okay-scope-4", "err:null-plus-string (scope resolution yields null)"),
    ("eval-okay-scope-6", "err:null-plus-string (scope resolution yields null)"),
    ("eval-okay-sort", "err:lessThan-expected-comparable-types"),
    ("eval-okay-substring-context", "err:getContext-expected-string (no string context)"),
];

fn lang_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // sui-bytecode/ -> workspace root
    p.push("sui-eval/tests/fixtures/lang");
    p
}

/// The active corpus — `eval-okay-*.nix` directly under `lang/`, matching the
/// walker's own non-recursive discovery so every engine sees the same set.
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

/// The vendored CppNix `.exp` — the third-party oracle, used to say which
/// engine is wrong when the two disagree on a value.
fn oracle(p: &Path) -> String {
    std::fs::read_to_string(p.with_extension("exp"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|e| format!("<no .exp: {e}>"))
}

/// Arm the strict latch. Must run before the first evaluation in this process.
fn arm_strict() {
    static ARM: Once = Once::new();
    ARM.call_once(|| {
        // SAFETY: called once, from the test's first statement, before any
        // worker thread is spawned and before any evaluation reads the latch.
        // No other test in this binary evaluates or touches the environment.
        unsafe { std::env::set_var("SUI_VM_STRICT", "1") };
    });
}

fn tree_outcome(p: &Path) -> Result<String, String> {
    sui_eval::builtins::clear_import_cache();
    let v = sui_eval::TreeWalkEvaluator
        .eval_file(p)
        .map_err(|e| e.to_string())?;
    render_tree(&v)
}

/// How the VM failed, kept typed so divergences classify themselves rather
/// than being sorted by hand from error text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fail {
    /// The compiler said, in its own words, that it cannot compile this. A
    /// loud refusal.
    UnsupportedCompile,
    /// The VM tried to hand an imported file (or the whole program) to the
    /// tree-walker and `SUI_VM_STRICT` refused. Also a loud refusal — and the
    /// one the latch exists to make visible.
    StrictRefusal,
    /// A runtime error that is neither of the above.
    Runtime,
}

fn vm_outcome(p: &Path) -> Result<String, (Fail, String)> {
    match sui_bytecode::eval_file(p) {
        Ok(r) => render_vm(&r.value, &r.interner).map_err(|e| (Fail::Runtime, e)),
        Err(e) => {
            let class = match &e {
                FileEvalError::Eval(EvalError::Compile(_)) => Fail::UnsupportedCompile,
                FileEvalError::Eval(EvalError::Runtime(r)) => {
                    // `fallback::record` formats its refusal with this prefix
                    // and propagates it as `VMError::Throw`.
                    if r.to_string().contains("SUI_VM_STRICT:") {
                        Fail::StrictRefusal
                    } else {
                        Fail::Runtime
                    }
                }
                FileEvalError::Read { .. } => Fail::Runtime,
            };
            Err((class, e.to_string()))
        }
    }
}

/// One fixture's verdict, retained so the guards below can re-inspect the run
/// instead of re-evaluating the corpus three times.
struct Row {
    name: String,
    tree: Result<String, String>,
    vm: Result<String, (Fail, String)>,
    /// `Layer::Builtin` crossings attributable to this fixture — the discount
    /// on "the VM computed this".
    builtin_bridges: u64,
    agree: bool,
}

/// One corpus run, plus the fallback counters attributable to exactly that run.
struct Corpus {
    rows: Vec<Row>,
    /// Whether the strict latch was armed for this run.
    strict: bool,
    builtin: u64,
    imported_file: u64,
    whole_expression: u64,
}

/// The corpus, evaluated **once per process** and shared by every test here.
///
/// Not an optimization — a correctness fix, and it was caught by a red run.
/// `fallback`'s counters are process-global `AtomicU64`s, so two tests each
/// running the corpus concurrently interleave their increments: the first
/// version of this file reported `builtin=8` when two tests ran and `builtin=4`
/// when one did, and the per-row `before`/`after` delta that attributes a
/// bridge crossing to a fixture was being computed across another test's
/// crossings. `OnceLock::get_or_init` serializes the racers onto one run, so
/// the counters describe that run and nothing else.
fn corpus() -> &'static Corpus {
    static CORPUS: std::sync::OnceLock<Corpus> = std::sync::OnceLock::new();
    CORPUS.get_or_init(|| {
        arm_strict();
        on_worker(|| {
            // Zero first, so the counters below are this run's and not a
            // leftover from whatever else touched the VM in this process.
            fallback::reset();
            let rows = run_corpus();
            Corpus {
                rows,
                strict: fallback::strict(),
                builtin: fallback::count(Layer::Builtin),
                imported_file: fallback::count(Layer::ImportedFile),
                whole_expression: fallback::count(Layer::WholeExpression),
            }
        })
    })
}

fn run_corpus() -> Vec<Row> {
    // THREAD-LOCAL, and that is load-bearing: `set_builtin_bridge`,
    // `set_flake_resolver` and `set_path_materializer` all store into
    // `thread_local!` cells (`sui-bytecode/src/bridge.rs`). Installing them on
    // the test's main thread would leave this worker with none, and the VM
    // would fail every fixture that needs a bridged builtin — a divergence
    // count made of harness error. They must be installed HERE.
    //
    // `install_vm_bridges` is the single production install site; hand-rolling
    // the three installs is banned precisely because a previous copy omitted
    // one and shipped a broken binary beside a green test.
    let _bridges = sui_eval::install_vm_bridges();

    let mut rows = Vec::new();
    for path in fixtures() {
        let name = stem(&path);
        let tree = tree_outcome(&path);

        let before = fallback::count(Layer::Builtin);
        let vm = vm_outcome(&path);
        let builtin_bridges = fallback::count(Layer::Builtin).saturating_sub(before);

        let agree = match (&tree, &vm) {
            (Ok(a), Ok(b)) => a == b,
            // Both refusing the same program is parity: demanding a value
            // would bias the corpus toward whatever both happen to implement.
            (Err(_), Err(_)) => true,
            _ => false,
        };
        rows.push(Row {
            name,
            tree,
            vm,
            builtin_bridges,
            agree,
        });
    }
    rows
}

/// Run the corpus on a big-stack worker, the way the CLI does (`main.rs`
/// spawns 256 MB for eval). Several fixtures recurse deeply enough to blow a
/// test thread's default 2 MB stack.
fn on_worker<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(f)
        .expect("spawn corpus worker")
        .join()
        .expect("corpus worker panicked")
}

#[test]
fn vm_corpus_matches_the_tree_walker() {
    let c = corpus();
    let rows = &c.rows;

    // ── ANTI-VACUITY, ALL OF IT BEFORE THE VERDICT ──────────────────────
    //
    // A floor placed after the loop lets an emptied set read green.

    // (a) The latch actually armed. Without this the whole run is the
    //     tree-walker answering for the VM, reported as VM coverage.
    assert!(
        c.strict,
        "SUI_VM_STRICT did not arm — every fallback below would be silent and \
         this run would report the tree-walker's coverage as the VM's. Refusing \
         to report a number that cannot be trusted."
    );

    let fixtures = fixtures();

    // (b) Discovery found the corpus. A clean result over a broken scan is not
    //     a clean result.
    assert!(
        fixtures.len() > 100,
        "found only {} lang fixtures — discovery is broken",
        fixtures.len()
    );

    // (c) The denominator travels with the verdict: the row set must match the
    //     scan, so a run that silently processed fewer fixtures fails.
    assert_eq!(
        rows.len(),
        fixtures.len(),
        "scanned {} fixtures but produced {} rows",
        fixtures.len(),
        rows.len()
    );

    let mut matched_value = 0usize;
    let mut matched_both_error = 0usize;
    let mut bridged_agreements = 0usize;
    let mut gaps = 0usize;
    let mut failures: Vec<String> = Vec::new();

    // Divergence classes.
    let mut declared_compile = 0usize;
    let mut declared_strict = 0usize;
    let mut undeclared_error = 0usize;
    let mut wrong_value = 0usize;
    let mut vm_only_success = 0usize;

    for r in rows {
        if r.agree {
            if r.tree.is_ok() {
                matched_value += 1;
                if r.builtin_bridges > 0 {
                    bridged_agreements += 1;
                }
            } else {
                matched_both_error += 1;
            }
        } else {
            match (&r.tree, &r.vm) {
                (Ok(_), Err((Fail::UnsupportedCompile, _))) => declared_compile += 1,
                (Ok(_), Err((Fail::StrictRefusal, _))) => declared_strict += 1,
                (Ok(_), Err((Fail::Runtime, _))) => undeclared_error += 1,
                (Ok(_), Ok(_)) => wrong_value += 1,
                (Err(_), Ok(_)) => vm_only_success += 1,
                (Err(_), Err(_)) => unreachable!("both-error is agreement"),
            }
        }

        match (gap_reason(&r.name), r.agree) {
            (Some(_), false) => gaps += 1,
            (Some(reason), true) => failures.push(format!(
                "  {}: listed in KNOWN_GAPS ({reason}) but now AGREES — remove \
                 the entry. An allowlist nobody prunes becomes the coverage number.",
                r.name
            )),
            (None, true) => {}
            (None, false) => {
                let class = match (&r.tree, &r.vm) {
                    (Ok(_), Err((Fail::UnsupportedCompile, _))) => "DECLARED/compile",
                    (Ok(_), Err((Fail::StrictRefusal, _))) => "DECLARED/strict-refusal",
                    (Ok(_), Err((Fail::Runtime, _))) => "UNDECLARED-ERROR",
                    (Ok(_), Ok(_)) => "WRONG-VALUE",
                    _ => "VM-ONLY-SUCCESS",
                };
                let vm_text = match &r.vm {
                    Ok(v) => v.clone(),
                    Err((_, m)) => format!("ERR {m}"),
                };
                let tree_text = r
                    .tree
                    .as_ref()
                    .map_or_else(|e| format!("ERR {e}"), Clone::clone);
                let mut entry = format!(
                    "  [{class}] {}:\n      walker: {tree_text}\n      vm:     {vm_text}",
                    r.name
                );
                // The `.exp` is the third-party oracle — it decides which
                // engine is wrong, so a wrong value is never adjudicated by
                // one of the two parties to the disagreement.
                if matches!((&r.tree, &r.vm), (Ok(_), Ok(_))) {
                    entry.push_str(&format!("\n      .exp:   {}", oracle(&lang_dir().join(format!("{}.nix", r.name)))));
                }
                failures.push(entry);
            }
        }
    }

    eprintln!(
        "\nVM vs tree-walker on the lang corpus (SUI_VM_STRICT=1): \
         {}/{} agree ({matched_value} value, {matched_both_error} both-error), {gaps} known gaps\n\
         \x20 of the {matched_value} value-agreements, {bridged_agreements} used >=1 tree-walker \
         builtin bridge (architectural, not failure — but not 'the VM computed this' either)\n\
         \x20 divergence classes: DECLARED/compile={declared_compile} \
         DECLARED/strict-refusal={declared_strict} UNDECLARED-ERROR={undeclared_error} \
         WRONG-VALUE={wrong_value} VM-ONLY-SUCCESS={vm_only_success}\n\
         \x20 fallback counters for THIS run: builtin={} imported-file={} \
         whole-expression={}\n",
        matched_value + matched_both_error,
        rows.len(),
        c.builtin,
        c.imported_file,
        c.whole_expression,
    );

    // The two failure-shaped layers must be zero, and under `SUI_VM_STRICT`
    // they are zero *by construction* — crossing either is an error, so a
    // crossing would have turned the row into a divergence rather than an
    // agreement. Asserting it anyway states the claim the coverage number
    // depends on: none of the 77 agreements is the tree-walker's answer
    // wearing the VM's name.
    assert_eq!(
        (c.imported_file, c.whole_expression),
        (0, 0),
        "the VM delegated a whole file or the whole expression to the \
         tree-walker; under strict that should have errored, so either the \
         latch is not doing its job or these counts are stale"
    );

    // (d) Agreements must rest on VALUES, not on both engines erroring in
    //     unison. Floor set below the measured figure so adding a fixture is
    //     never a two-file edit, while a collapse is still caught.
    assert!(
        matched_value >= 30,
        "only {matched_value} of {} rows agreed on a VALUE; the rest agreed by \
         both failing. Two engines erroring in unison is not parity evidence.",
        rows.len()
    );

    assert!(
        failures.is_empty(),
        "\n{} of {} lang fixtures diverge between the bytecode VM and the tree-walker:\n{}",
        failures.len(),
        rows.len(),
        failures.join("\n")
    );
}

/// No agreement rests on a placeholder.
///
/// `<<lambda>>`, `<<builtin n>>` and the depth sentinel `<...>` are the three
/// strings both engines can emit for a value whose *contents* were never
/// compared. `render_vm` already refuses an unforced thunk and `render_tree`
/// propagates force errors, so those cannot reach here — this closes the
/// residual: two real functions rendering identically without their bodies
/// having been checked.
///
/// Measured: **0** agreeing rows contain any of the three, so every reported
/// agreement is on a fully concrete value.
#[test]
fn no_agreement_rests_on_a_placeholder() {
    const PLACEHOLDERS: &[&str] = &["<<lambda>>", "<<builtin ", "<...>"];
    let c = corpus();
    assert!(c.strict, "SUI_VM_STRICT did not arm");

    // Denominator inside the compared value: a run that agreed on nothing
    // would otherwise satisfy "no agreement rests on a placeholder" vacuously.
    let agreeing: Vec<&Row> = c.rows.iter().filter(|r| r.agree && r.tree.is_ok()).collect();
    assert!(
        agreeing.len() >= 30,
        "only {} value-agreements to check — this guard is vacuous below that \
         and must not report clean",
        agreeing.len()
    );

    // Scan BOTH renders, not just the walker's.
    //
    // This loop read `r.tree` alone and the doc above claimed it "re-renders
    // every agreeing row" — it did neither. For an (Ok, Ok) agreement the two
    // strings are equal, so scanning one was not a false green; but it proved
    // nothing whatever about `render_vm`, which is the render this file exists
    // to trust. A guard that never looks at the engine under test is a guard
    // about the oracle.
    let mut tainted: Vec<String> = Vec::new();
    for r in agreeing {
        // `tree` and `vm` carry different Err types, so both are narrowed to
        // `Option<&str>` — the error side is irrelevant here, only the
        // rendered text matters.
        for (side, rendered) in [
            ("walker", r.tree.as_deref().ok()),
            ("vm", r.vm.as_ref().map(String::as_str).ok()),
        ] {
            let Some(rendered) = rendered else { continue };
            for ph in PLACEHOLDERS {
                if rendered.contains(ph) {
                    tainted.push(format!(
                        "  {} [{side}]: agreement contains {ph} — {rendered}",
                        r.name
                    ));
                    break;
                }
            }
        }
    }
    assert!(
        tainted.is_empty(),
        "{} agreeing fixtures agree only on a placeholder; two placeholders \
         compare EQUAL, so these are not evidence either engine produced a \
         value:\n{}",
        tainted.len(),
        tainted.join("\n")
    );
}

/// Every allowlist entry must name a fixture that is actually in the corpus.
///
/// Carried over from `sui-ir/tests/lang_corpus_ir.rs`, where it exists because
/// a red-run failed to go red: an entry naming a fixture the scan never sees is
/// silently inert, indistinguishable from one suppressing a real gap.
#[test]
fn every_known_gap_names_a_real_fixture() {
    let present: Vec<String> = fixtures().iter().map(|p| stem(p)).collect();
    assert!(
        present.len() > 100,
        "discovery found {} fixtures — this guard cannot certify an allowlist \
         against a corpus it failed to read",
        present.len()
    );
    let phantom: Vec<&str> = KNOWN_GAPS
        .iter()
        .map(|(n, _)| *n)
        .filter(|n| !present.iter().any(|p| p == n))
        .collect();
    assert!(
        phantom.is_empty(),
        "these KNOWN_GAPS entries name no active fixture: {phantom:?}. Either \
         the name is a typo, or the fixture moved — in which case delete the \
         entry, because it is suppressing nothing."
    );
}

/// The allowlist is pinned, so growing it is a deliberate edit.
#[test]
fn the_known_gap_list_is_pinned() {
    assert_eq!(
        KNOWN_GAPS.len(),
        45,
        "KNOWN_GAPS changed size. It may SHRINK freely — delete the entry and \
         update this number. Growing it means the VM regressed against the \
         corpus, or a newly-vendored fixture was allowlisted instead of fixed; \
         either way say which in the commit."
    );
}

/// The three engines' render depth caps must agree, or a deep value diverges
/// for a reason that is purely about rendering.
#[test]
fn the_render_depth_cap_is_shared() {
    assert_eq!(
        sui_bytecode::render::MAX_RENDER_DEPTH,
        sui_eval::render::MAX_RENDER_DEPTH
    );
    assert_eq!(
        sui_bytecode::render::DEEP_SENTINEL,
        sui_eval::render::DEEP_SENTINEL
    );
}

/// `escape_str` is duplicated across the two crates and MUST agree byte for
/// byte, because the differential compares rendered strings.
///
/// It was pinned nowhere. The sibling constants above got a real cross-crate
/// comparison; the one piece of duplicated *logic* got none, and the in-crate
/// test that looked like its pin
/// (`render.rs::the_depth_cap_matches_the_other_engines`) compares
/// `MAX_RENDER_DEPTH` against a local function returning the literal `128` —
/// it asserts `128 == 128` and cannot see sui-eval at all.
///
/// A divergence here is the worst kind for this file: it would surface as a
/// corpus row DIVERGING on a value both engines computed identically, sending
/// the reader after an evaluator bug that does not exist.
#[test]
fn escape_str_is_byte_identical_across_the_engines() {
    // Every class the renderers can disagree on: the escapes themselves, the
    // characters adjacent to them, and a non-ASCII case where a naive
    // byte-wise implementation and a char-wise one part company.
    let cases = [
        "",
        "plain",
        "with \"quotes\"",
        "back\\slash",
        "new\nline",
        "tab\there",
        "carriage\rreturn",
        "dollar${interp}",
        "mixed \"a\\b\nc\"",
        "unicode: é 日本語 🙂",
        "\u{1}\u{7f}",
    ];
    for c in cases {
        assert_eq!(
            sui_bytecode::render::escape_str(c),
            sui_eval::render::escape_str(c),
            "escape_str diverges on {c:?} — a rendered-string differential \
             would report this as an EVALUATOR divergence"
        );
    }
}
