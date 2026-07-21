All grounding gathered (benches/BASELINES.md, benches/VS-NIX-HARNESS.md, sui-spec/specs/perf.lisp, the CLI's `parity`/`build-parity`/`perf-seal` surfaces). The spec follows, ready to land verbatim at `/Users/drzzln/code/github/pleme-io/sui/benches/USE-CASE-MATRIX.md`.

# USE-CASE BENCHMARK MATRIX — the falsifiable form of "fastest across all use cases"

> **Status:** SPEC (matrix defined; harness partially shipped). Grounded in the
> 2026-07-21 cid live-profile campaign, `benches/VS-NIX-HARNESS.md` (2026-07-17
> run), and the typed perf ledger `sui-spec/specs/perf.lisp`. Dates 2026.

"Fastest across all use cases" is a **claim with a per-class oracle**, not a
headline. This document defines it the fleet's verification-matrix way
(★★ CLOSED-LOOP MASS-SYNTHESIS, Rule 1): **one matrix, one row per use-case
class, CI-runnable, aggregate-failing** — a red row when a class regresses, a
red build when a new class lands without a row. The 2026-07-17 harness already
proved why a single number lies: sui is **22.7× faster** on let-chains and
**9.4× slower** on deep recursion *in the same run*. The matrix is the honest
replacement for the "3× CppNix" headline.

## I. The claim, decomposed into gates

Every row is graded on a three-tier gate ladder. The headline claim is defined
as **every row at G2**; today's scoreboard (§IV) shows exactly which rows are
where. Never round up: a row's tier is what its evidence earns.

| Gate | Meaning | Red condition |
|---|---|---|
| **G0 — completes** | sui finishes the workload at all, inside a memory ceiling | non-zero exit, OOM/swap-death, or footprint > row ceiling |
| **G1 — no-regress** | ≤ committed baseline (deterministic work counters where available, wall ± noise band where not) | work count up, or wall outside band vs `benches/use-case-baseline.json` |
| **G2 — beats oracle** | sui ≤ nix on the row's *honest metric* | ratio > 1.0 on the honest metric |

Parity is a **precondition, not a gate here**: every harness run requires the
corpus green (`sui parity` — 77/77, exit 0) before any perf number is recorded.
A fast wrong answer is not a row.

## II. Honesty rules (bind every row)

These are inherited from `VS-NIX-HARNESS.md` and `sui-spec/specs/perf.lisp`;
they are what keep the matrix from lying.

1. **Release profile only.** Debug numbers are 5–20× off; the results JSON
   records `profile` and CI rejects `debug` rows.
2. **Tree-walker only (`--no-vm`).** The tree-walker is the parity engine; the
   VM defers string-context and is not parity-capable. A VM column may be added
   later as *informational-only*, never gated.
3. **Spawn-floor discipline.** CppNix's process spawn floor is ~70 ms on this
   machine. Sub-millisecond shapes use `engine_ratio = (nix_wall − spawn_floor)
   / sui_wall`, valid only when `nix_eval ≥ 500 µs`
   (`MEANINGFUL_CPPNIX_EVAL_US`). **Exception:** U01 (repl latency) is *about*
   startup — its honest metric is raw wall, spawn included, for both engines.
4. **Memory = footprint, not RSS.** The cid death mode is **swap exhaustion at
   linear ~140 MB/s retained allocation with only ~11% resident** (64 GB
   footprint / 7 GB RSS at 8 min; ~90% of writable pages written once and never
   touched again). `max_rss` alone would grade a dying eval as healthy. Rows
   with a memory ceiling sample **phys_footprint** (macOS `footprint`/task
   info), and record both.
5. **Interleaved A/B for wall claims.** One-sided before/after numbers are how
   `overlay-base-move`'s false "8.9%→0.08%" happened (mimalloc-vs-system-malloc
   apples-to-oranges). Wall deltas ship only from interleaved A/B rounds.
6. **A profile share is not a wall number.** `canonicalize-memo` cut the
   `__getattrlist` sample share 61%→~2% and is still honestly `measured:
   Pending` in the ledger because no end-to-end wall exists. The matrix records
   `share` and `wall` as separate fields; only `wall` feeds G1/G2.
7. **Force-order changes need a force-order proof.** Per the ledger's Technique
   taxonomy: `ReprSwap`/`MemoizeIdempotentQuery` classes are established by
   byte-identical corpus (ByteSufficient); anything that changes force order
   (`ForceOrderChange`) requires the force-order proof tier, and the corpus
   must stay 77/77 either way.
8. **Proven dead ends are fenced.** A red row's remediation may **not**
   re-attempt levers the ledger already Discarded with a named ceiling:
   `thunk-env-repr` and `positional-frames` (ceiling `PersistentLazyDesign` —
   frame alloc > HAMT probe; the ~9× deep-recursion gap is the price of the
   no-GC persistent-lazy Rc design), `overlay-merge-structural`,
   `thunk-waste-elision` (byte-safe eager-elidable fraction ≈ 0%). Read
   `sui-spec/specs/perf.lisp` before proposing a fix.

## III. The matrix — twelve classes

Machine baseline: cid (macOS aarch64, M-series), sui tree-walker release,
nix 2.34.7 oracle. All oracle numbers below are **measured ground** unless
marked *(to capture)*.

### U01 — tiny expr (repl / CLI latency)

| | |
|---|---|
| **Product meaning** | interactive `-E` one-liner; what a shell user feels |
| **Oracle** | `nix-instantiate --eval -E '1 + 1'` — wall ≈ the ~70 ms spawn floor |
| **Harness** | `sui --no-vm eval --no-eval-cache -E '1 + 1'` (timed wall, spawn included) |
| **Honest metric** | raw wall, both sides (this class IS startup; rule 3 exception) |
| **Status** | **MEASURED (G2 green)** — sui wall is sub-ms; wall_ratio ~200–1700× on small shapes per the 07-17 harness |

### U02 — single-file eval (hot-shape mix)

| | |
|---|---|
| **Product meaning** | evaluating a mid-size standalone `.nix` file (no flake, no store) |
| **Oracle** | `nix-instantiate --eval <file>` minus spawn floor |
| **Harness** | `SUI_TEST_ONLINE=1 cargo test --release -p sui-eval --test vs_nix_hotshapes -- --nocapture` (shipped; emits `target/vs-nix-hotshapes.results.json`) |
| **Honest metric** | `engine_ratio` geomean over `engine_comparable` shapes |
| **Status** | **MEASURED** — release engine geomean **1.86×** (let_5 22.7×, foldl_100 2.86×, fib_10 1.72×); G2 green *except* the deep-recursion shape, split out as U05 |

### U03 — flake-eval small (kataFleet-class)

| | |
|---|---|
| **Product meaning** | an IFD-free flake output eval, ~tens of seconds (`nix eval .#kataFleet.report --json` in the nix repo) |
| **Oracle** | nix ≈ **35 s** today *(re-capture with the harness timer)* |
| **Harness** | `sui --no-vm eval --json --no-eval-cache .#kataFleet.report` from the nix repo checkout |
| **Honest metric** | cold wall (warm is U09) |
| **Status** | **MEASURED (cold)** — sui cold **35.2 s** ≈ oracle parity on wall; G2 = tie-to-marginal. Interleaved A/B row *(to capture)* to fix the exact ratio |

### U04 — nixpkgs leaf drvPath (hello)

| | |
|---|---|
| **Product meaning** | the canonical "instantiate one package" — eval through the store-path machinery |
| **Oracle** | `nix eval --raw --impure --extra-experimental-features "nix-command flakes" --expr '(import (builtins.getFlake "nixpkgs") { system = builtins.currentSystem; }).hello.drvPath'` |
| **Harness** | `sui --no-vm eval --raw --impure -E '…same expr…'` (byte-equality hard-asserted by `vs_nix_hotshapes`'s correctness section) |
| **Honest metric** | wall, plus byte-parity as precondition |
| **Status** | **PARITY GREEN** (hello + coreutils byte-equal, 07-17 run); **wall UNMEASURED** as an isolated timed row *(to capture — the drvPath rows in the harness assert bytes but do not time)* |

### U05 — deep-chain (the ~9× recursion gap)

| | |
|---|---|
| **Product meaning** | deep recursive call chains (fib-20-class; ~13.5k calls, ~26 recursive eval_expr/apply frames per stack) |
| **Oracle** | nix_eval **4,100 µs** (fib 20, spawn-subtracted) |
| **Harness** | shipped as the `rec_fib_20` row of `vs_nix_hotshapes`; hand-repro: `sui --no-vm eval --no-eval-cache -E 'let f = n: if n < 2 then n else f (n - 1) + f (n - 2); in f 20'` |
| **Honest metric** | engine_ratio |
| **Status** | **MEASURED (G2 RED)** — sui 38,428 µs, ratio **0.107×** (~9.4× slower). Ledger verdict: this gap is the *priced ceiling* of the persistent-lazy no-GC design (`thunk-env-repr` Discarded, `positional-frames` net-negative). Row gate = G1 only (no-regress) until a technique **outside** the fenced dead-end list exists; the named open axis is the **rowan AST re-walk** (40.7% bytes / 51% alloc calls / 21% wall self-time), not thunk/env repr |

### U06 — attrset-heavy (mkDerivation shapes)

| | |
|---|---|
| **Product meaning** | attrset construction/merge/select at mkDerivation density — the shape nixpkgs is made of |
| **Oracle** | `attrset_merge` micro: nix_eval 222 µs (sub-threshold, informational). A macro **mkDerivation-1k fixture** *(to author in `parity_corpus.rs`)* is the real oracle row |
| **Harness** | hot-shapes row today; the fixture row once authored. Work-counter gate via `sui perf-seal` |
| **Honest metric** | engine_ratio on the macro fixture; eval_expr work count for G1 |
| **Status** | **PARTIALLY MEASURED** — micro 5.29× (below the 500 µs comparability floor); `sym-keyed-attrs` Landed 07-21 (interner cluster 27–39% → ≤6.7% sample share; wall Pending); `attrset-symcache` (+25.4%) **Deferred on soundness** — do not resurrect without the source-id audit |

### U07 — IFD-bearing eval

| | |
|---|---|
| **Product meaning** | eval that must *build* mid-eval (import-from-derivation; the gen/tobira build-spec class — cache-untouchable by eval memo) |
| **Oracle** | `nix eval` with IFD allowed over a minimal fixture: `let d = derivation {…writes a .nix…}; in import d` *(fixture to author; then a real gen-build-spec row)* |
| **Harness** | `sui --no-vm eval` over the same fixture — requires build-during-eval through sui's store |
| **Honest metric** | G0 first (completes at all), then wall |
| **Status** | **UNVERIFIED** — the build machinery exists (`sui build-parity` proves byte-identical builds) but the IFD-during-eval path has no probe. This row exists precisely so that gap is a red cell, not an unknown |

### U08 — string-heavy

| | |
|---|---|
| **Product meaning** | string concat/interpolation-dominated eval (URL/path assembly, mass `+`) |
| **Oracle** | *(to capture — the string-heavy A/B workload has no nix-comparative row yet)* |
| **Harness** | promote the `string-concat` lever's foldl'-string workload into `vs_nix_hotshapes` as a named shape |
| **Honest metric** | engine_ratio |
| **Status** | **INTERNALLY MEASURED** — `string-concat` Proven +16.5% (interleaved A/B, ledger `speedup-bp 1650`); **oracle-comparative UNMEASURED** |

### U09 — REPEAT warm (eval-cache hit)

| | |
|---|---|
| **Product meaning** | re-running yesterday's eval on an unchanged tree — the daily-driver loop |
| **Oracle** | warm `nix eval .#kataFleet.report --json` (nix's own flake eval cache) *(to capture)* |
| **Harness** | second consecutive `sui --no-vm eval --json .#kataFleet.report` on a **clean** tree (cache is clean-git-rev-keyed, `--no-vm` only) |
| **Honest metric** | warm wall |
| **Status** | **MEASURED sui-side (G0/G1 green)** — **0.03 s vs 35.2 s cold** (~1170× repeat-memo win). G2 pending the nix warm number; expected green (nix's warm path re-evals far more) but *record it, don't assume it* |

### U10 — whole-system closure (cid marquee)

| | |
|---|---|
| **Product meaning** | the full darwin system: `.#darwinConfigurations.cid.system.build.toplevel.drvPath`, **20,827 drvs** |
| **Oracle** | nix: **~107 s quiet / ~3 GB** |
| **Harness** | `sui --no-vm eval --raw --no-eval-cache .#darwinConfigurations.cid.system.build.toplevel.drvPath` under a footprint sampler; row ceiling = **8 GB footprint** (≈ 2.5× oracle) with a hard swap-death timeout |
| **Honest metric** | G0 (completes) → peak footprint → wall |
| **Status** | **CANNOT-COMPLETE (G0 RED — the matrix's headline red cell).** sui has never finished it: ~140 MB/s **linear** retained allocation, no plateau; 64 GB footprint at 8 min with 7 GB resident; death by swap exhaustion, not jetsam. 51.8% of 7.8M thunks never forced, each pinning its captured Env HAMT; remaining CPU (post `sym-keyed-attrs`): memmove ~13%, mimalloc ~9%, path-Components ~5%, rowan cursor ~4.5%, `bitmaps::Iter` ~2.6%. Remediation targets the **retained-footprint** class (dead-weight pages / pinned Envs), inside the §II.8 fence |

### U11 — dirty-tree edit-rebuild

| | |
|---|---|
| **Product meaning** | edit one file, re-eval — the inner dev loop |
| **Oracle** | nix wall after touching one flake file *(to capture)* |
| **Harness** | touch a leaf `.nix` in the nix repo, re-run the U03 command |
| **Honest metric** | warm-after-edit wall |
| **Status** | **UNMEASURED — structurally COLD today.** The eval cache is *clean-git-rev-keyed*, so any dirty tree pays the full 35.2 s cold path (the row measures exactly this). Named destination: content/input-addressed cache keying so an edit invalidates only its cone. This row is the forcing function that keeps that destination visible |

### U12 — parallel-shard throughput

| | |
|---|---|
| **Product meaning** | many independent attr evals at once (CI matrix / nix-eval-jobs class) |
| **Oracle** | N parallel `nix eval` workers (or `nix-eval-jobs`) over disjoint attrs; attrs/s |
| **Harness** | N parallel `sui --no-vm eval` **processes** over the same shard list |
| **Honest metric** | aggregate attrs/s at N ∈ {1, 4, 8} |
| **Status** | **UNMEASURED.** Constraint stated honestly: sui eval is hard single-threaded (one sui-eval thread; 10 tokio workers parked), so within-process parallelism is not on the table — the honest row is *process*-level sharding, where the ~70 ms-free startup and per-process memory footprint (U10's pathology × N) both matter |

## IV. Scoreboard (2026-07-21 — the honest starting position)

| Row | Class | Oracle | sui | Verdict |
|---|---|---|---|---|
| U01 | tiny expr | ~70 ms | sub-ms | **WIN** (G2) |
| U02 | single-file mix | — | geomean 1.86× | **WIN** (G2, ex-U05) |
| U03 | flake-eval small | ~35 s | 35.2 s cold | **TIE** (A/B to fix) |
| U04 | hello drvPath | bytes ✓ | bytes ✓ | **PARITY** / wall to capture |
| U05 | deep-chain | 4.1 ms | 38.4 ms | **LOSS 9.4×** (priced ceiling) |
| U06 | attrset-heavy | 222 µs† | 42 µs† | partial (macro row to author) |
| U07 | IFD-bearing | — | — | **UNVERIFIED** |
| U08 | string-heavy | — | +16.5% internal | oracle row to capture |
| U09 | repeat warm | *(capture)* | **0.03 s** | **WIN** pending oracle |
| U10 | whole-system (cid) | 107 s / 3 GB | **never completes** | **CANNOT-COMPLETE** |
| U11 | dirty-tree edit | *(capture)* | = cold (35.2 s) | structurally cold |
| U12 | parallel-shard | *(capture)* | *(capture)* | UNMEASURED |

† sub-comparability-threshold micro numbers, informational only.

**Reading:** the claim is green on the interactive/shallow/warm classes, tied
on small flake eval, red on deep recursion (priced), and **red at G0 on the
one class that is the actual product** (U10). U10 and U07 are the two cells
that gate any "fastest across all use cases" statement; U05 is honest-red with
a named ceiling.

## V. CI wiring — the forcing function

- **Harness home:** extend `sui-eval/tests/vs_nix_hotshapes.rs` (typed rows,
  serde results, spawn-floor + comparability logic already proven) rather than
  a new binary; new fixtures land in `src/parity_corpus.rs`. NO-SHELL: rows
  are Rust `#[test]`s + typed subprocess wrappers, gated by `SUI_TEST_ONLINE=1`.
- **Matrix test** (`sui-eval/tests/use_case_matrix.rs`, fleet pattern):
  `const MATRIX: &[UseCaseRow]` with one row per U-class; failures aggregate
  before the assert (one run reports every red row); a companion
  `matrix_covers_all_use_case_classes` test pins `MATRIX.len() >= 12` so a new
  class without a row fails the build.
- **Baselines:** `benches/use-case-baseline.json`, minted/graded the
  `sui perf-seal` way (deterministic work counters where available — G1 reds
  are "does more eval work", never runner noise; wall rows carry an explicit
  noise band). Re-mint only with `--write-baseline` + a commit.
- **Preconditions per run:** `sui parity` 77/77 exit 0; `profile == "release"`;
  results JSON records `{row, profile, wall_us, engine_ratio, peak_footprint_mb,
  max_rss_mb, work_counters, gate: G0|G1|G2, verdict}` to
  `target/use-case-matrix.results.json` + an md mirror.
- **Cadence:** U01–U06, U08–U09 on every PR (seconds-scale); U03/U09/U11
  nightly against the nix repo checkout; U10/U12 nightly with the footprint
  sampler and swap-death timeout (a timeout is a **recorded CANNOT-COMPLETE
  row**, not a flake).
- **Ledger coupling:** any lever that flips a row's verdict lands a
  `defperf-lever` entry in `sui-spec/specs/perf.lisp` in the same commit;
  a wall claim without an interleaved A/B stays `measured: Pending` in both.