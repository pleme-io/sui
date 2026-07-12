# sui eval-perf-seal — the honest plan (measure, box, seal; no macro)

> Big-bang recon + adversarial verify (2026-07-12) against `sui` `main`. **Three
> overclaims in the framing were caught by the adversarial pass and are recorded in §7
> so they never silently return.** The headline corrections: (a) there is NO earned
> macro vocabulary — the storms are 3 non-uniform impl classes; (b) the model-level
> seat is authored-but-**engine-unwired** (a pending destination, not today's state);
> (c) the deterministic op-count regression box **already ships** — the work is to
> WIDEN it and add the byte gate to CI.

## 1. Destination

sui eval is *provably fast* the way it is *provably byte-correct* — **two deterministic
CI gates, each red on regression of its committed corpus:**
- **`perf-seal`** (SHIPPED, `perf-seal.yml` + `perf_seal.rs` + `perf-baseline.json`) —
  grades each eval shape on a **deterministic `EvalExpr` op-count** vs baseline (±15%);
  wall-clock is report-only, never a gate (a shared-runner ms budget would flake).
- **`parity`** (byte gate, `parity.yml`, NEW this commit) — diffs each shape's sui
  drvPath/hash/NAR byte-for-byte against `nix`.

Together they box both axes: **eval may not get slower AND may not change a byte.** The
58/102-match byte floor is sacred; every perf move lands byte-neutral behind the byte
gate or it does not land.

## 2. The honest verdicts (never round up)

| Directive framing | Verdict | Corrected |
|---|---|---|
| "Solve at the MODEL level, both engines drive it" | **overclaim** | The `laziness`/`coercion` sui-spec is a SHIPPED typed *assertion* surface but **engine-UNWIRED** — neither engine calls its `force`/`apply` (only `derivation.rs` genuinely drives its spec via `load_canonical()`+`apply()`). Session fixes are hand-written Rust (`merge_deferred_dynamic_tail`, `canon_abs`, Blackhole→Promise). "Model level" is a **pending destination** (BUILD.md §II marks it pending), realized by *wiring* the engine to the spec — not a macro. |
| "Produce a macro vocabulary to solve" | **overclaim** | The storms partition into **3 non-uniform classes** (compute-once/content-address ×2; string-context propagation family; force-order/attrset-merge) — no single `(defmemo)` collapses them, and one would pollute the force hot-path (the mado over-abstraction rejection). Honest answer: targeted memoizations + the typed-spec DOMAINS that already exist. The refactor holds eval for LAST on purpose. |
| "Box it in provably (a regression gate)" | **already shipped, partial** | The deterministic op-count box EXISTS (`perf-seal`). It covers 8 micro/storm shapes, **NOT** the ~39/40-min marquee cost (that's in the un-instrumented hot path), and the **byte gate was not in CI** (fixed by `parity.yml` this commit). Two gates over partial corpora — not one "provable" box. |

## 3. The dominant storm — UNMEASURED, two structural suspects

Profiler not yet run; dominance is a code-derived best-guess.
- **Storm A — O(N²) recursion detection (strongest suspect).** `is_self_recursive_binding`
  (`eval.rs:575-596`) walks each binding's full RHS subtree against every sibling name,
  in `eval_attrset`-rec + `eval_let`, re-done per fixpoint iteration → O(N²) per scope,
  super-linear on the M2.6 module let-scope (`eval.rs:1137-1141` names the victim).
  **Zero perf counters** → matches "~39 of 40 min in the un-traced path."
- **Storm B — overlay re-flatten.** Each `//` mints a fresh `Overlay` with a cold
  `Rc<OnceCell>` (`value.rs:1581/1616-1637`); a re-derived fixpoint gets cold caches each
  iteration (`perf.rs:80-91` names the leak). Has counters — the `OverlayFlattenBuild/
  Attempt` ratio is the confirming signal.

**Decisive cheap experiment (M1):** `SUI_EVAL_PERF=1 sui eval` on cid → read the
overlay-flatten block; add ONE counter at `eval.rs:589` for A; compare. Do not fix
before measuring.

Byte-neutral grading of the candidate fixes: Fix A (memoize the recursion classifier per
rowan node-id — pure fn of subtree+sibling-set), Fix B (content-key the flat cache across
iterations) = **ByteNeutral by construction** (still byte-verify A against the nix oracle
— it touches the M2.6 classifier). Fix C (thunk content-memo) = **RISKY, deferred**
(byte-neutral only if content-keyed + force-free-to-key).

## 4. Model seat + macro — the honest answer

**2–3 targeted memoizations behind existing surfaces; NO macro; NO new doctrine.**
- No `(defmemo)`/`(defsharing)` macro (3 non-uniform classes; hot-path pollution; fails
  the ≥3-real-reuse EMITTER-SUBSTRATE bar).
- The `laziness`/`coercion` specs stay the SHIPPED assertion surface; new roots land as
  hand-written Rust *documented* by a `(defthunk-discipline)` row — authoring the row is
  edit-safe but does NOT by itself make the engine obey the spec.
- **The one genuinely-new model surface is DESTINATION (M4), not a macro:** wire the
  engine force/classify path (or a CI equivalence gate: engine classifier output ==
  spec `is_parity_correct` per site) to the authored spec, `derivation.rs`-style, so
  drift is unrepresentable rather than merely asserted. Gated on measurement.

## 5. Phases

- **M0 — both gates in CI.** `perf-seal` shipped; `parity.yml` added this commit (byte
  gate; installs nix; linux corpus). Acceptance: both green in CI, byte floor intact.
  *(parity.yml needs a first CI validation run — nix-in-CI env may need tuning; that's
  iterating on a real gate, the honest boxing move.)*
- **M1 — MEASURE (gate before any fix).** `SUI_EVAL_PERF=1` on cid + a counter around
  `is_self_recursive_binding`; read `eval_cache.rs`/`lazy.rs`/`drv_cache.rs` (Import
  counters are declared-but-never-incremented). Output: a measured A-vs-B verdict.
- **M2/M3 — fix the dominant storm byte-neutrally + seal behind the byte gate.** Land the
  fix, byte-verify pre/post against nix (Parity Method), regression-test, widen the
  perf-seal corpus toward the marquee eval, commit.
- **M4 (DESTINATION) — DRIVE the spec.** Wire the engine → the authored `laziness`/
  `coercion` spec so drift is unrepresentable. The honest "solved at model level" is
  *this*, pending.

## 6. Reused vs new

**Reused:** the shipped `perf-seal` op-count gate + baseline; `EvalExpr`/`OverlayFlatten*`
counters; the `parity` corpus + `ParityVerdict`; `as_flat`/`ensure_flat`; the
`laziness`/`coercion` assertion domains. **New (small):** `parity.yml` (byte gate in CI);
one `is_self_recursive_binding` counter; one byte-neutral storm fix; and — DESTINATION —
the engine↔spec wiring.

## 7. Claims that must NOT round up

1. Eval fixes are **hand-written Rust documented by a spec**, NOT "a model both engines
   drive." "Solved at model level" is a **pending destination**.
2. A macro vocabulary is **NOT earned** — 3 non-uniform classes; targeted memoizations +
   typed-spec domains, not one derive over eval.
3. **No single "provable" gate boxes both** — two deterministic-count gates over PARTIAL
   corpora; the speed gate does NOT cover the dominant cid cost.
4. Dominance (Storm A vs B) is **UNMEASURED** — profile first.
5. Fix C and any `(defsharing-collapse)` domain are **design/deferred**, not in-hand.
6. No doctrine name minted — no substrate earned (anti-premature-mint).
