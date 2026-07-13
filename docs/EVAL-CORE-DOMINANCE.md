# sui eval-core dominance — M0 / Gate 0.5: the measured baseline

> Byte-neutral instrumentation + a SYMMETRIC measured profile of the eval-core
> perf gap (2026-07-12), on `sui` `main` (`sui parity` = 64 match). This M0
> **chooses the M2 lever by measurement**, it does NOT fix code. Companion to
> [`EVAL-PERF-SEAL.md`](./EVAL-PERF-SEAL.md) §3 (which named this exact M1 —
> "add ONE counter around `is_self_recursive_binding` … MEASURE before any fix")
> and [`PERF-ARSENAL.md`](./PERF-ARSENAL.md).
>
> **Prime result:** on the deep workload the eval gap is **~6× wall / ~10×
> memory / ~10× instructions** — and *neither* named algorithmic storm (Storm A
> recursion-detection, Storm B overlay-flatten) is the dominant cost. The eval
> is **apply/force/thunk-allocation-bound**, with **51.8% thunk waste** and
> **61% redundant thunk-stores** as the biggest closable terms. Storm A and
> overlay-flatten are each ≤2.1% of the deep eval.

---

## 1. What this M0 shipped (all byte-neutral)

- **One symmetric counter around the residual Storm-A walk.** `referenced_idents`
  (`eval.rs`, the function `is_self_recursive_binding` delegates to — the O(N²)
  per-`(binding × sibling)` re-walk was already hoisted to O(N) per scope) now
  emits `SelfRecWalkCalls` (# binding-RHS subtree walks), `SelfRecWalkNodes`
  (total rnix `descendants()` visited), and a walltime accumulator — exactly the
  shape of the shipped `sorted_entries` / `overlay_flatten` counters. **Storm A
  is now VISIBLE in the `SUI_EVAL_PERF=1` report**, so the Storm-A-vs-overlay
  comparison is symmetric (before this, Storm A had zero counters — you could not
  compare an instrumented term to an uninstrumented one).
- **Byte-neutral by construction:** the counter reads sit behind `perf::enabled()`
  and add zero output-relevant work; the `set` computed by `referenced_idents` is
  identical. **`sui parity` = 64 match before and after** (see §6).

## 2. The measured gap (tier: RELEASE build, tree-walker `--no-vm`)

Build tier is stated because it is load-bearing: the fabricated "20×" wall figure
in prior framing was a **debug-build artifact**. All numbers below are `cargo
build --release` + `sui --no-vm eval` (the tree-walker, sui's byte-parity engine).

### 2.1 The DEEP workload (the one that matters)

Expr (self-contained, reproducible; the M2.6 marquee module-fixpoint):
```
let n = builtins.getFlake "<nixpkgs-store-path>";
in (n.lib.nixosSystem { system = "x86_64-linux"; modules = []; }).config.system.name
```
This drives the full module-system let-scope + option fixpoint (nixpkgs
`baseModules`, ~1982 modules) — the deep let-scope Storm A lives in and the
closest reproducible proxy for the cid darwin closure. **Both engines return
`"nixos"` — byte-identical.** (The bounded slice is used because the full cid
darwin toplevel is blocked by an unrelated IFD in this repo's own flake; see §5.)

| Metric | sui (release, no perf, best-of-2) | nix (cold) | ratio |
|---|---:|---:|---:|
| wall (real) | 10.63 s | 1.73 s | **6.1×** |
| user+sys | 7.22 s | 0.82 s | ~8.8× |
| maxRSS | 1975 MB | 191 MB | **10.3×** |
| instructions retired | 67.3 G | 7.05 G | **9.5×** |

> Compared to the hello probe the task measured (~4× wall / 37× mem): the DEEP
> workload's **wall gap is WORSE** (6.1× vs 4×) because the deep let-scope hammers
> the apply/force/thunk machinery, while its **memory ratio is LOWER** (10.3× vs
> 37×) because the deep workload amortizes sui's fixed per-process overhead that
> dominates hello. The gap is workload-shape-dependent; "37×" is a small-workload
> ceiling, not the marquee.

### 2.2 The counter breakdown on the deep workload — which counter dominates

Full `SUI_EVAL_PERF=1` report (elapsed 13.15 s *with* counters on):

```
eval_expr:      3,688,836     force_value: 2,397,297    thunk_forces: 448,872
apply:          1,052,524     select:        327,909    attrsets:     119,690
env_clones:       658,289     env_lookups: 1,354,096    imports: 2391 (238 cached)
--- expression breakdown ---
  ident   1,245,435 (33.8%)   apply 736,396 (20.0%)   select 327,909 (8.9%)
  lambda    328,516  (8.9%)   binop 277,218  (7.5%)   string 231,304 (6.3%)
  if-else   178,499  (4.8%)   attrset 119,690 (3.2%)  let-in  61,756 (1.7%)
--- overlay (`//`) flatten ---   builds 2273 / attempts 12954 (82.5% hit)
  flatten_walltime:  272.7 ms   (2.1% of eval)
--- sorted_entries ---           calls 75526   walltime 7.7 ms (0.1%)
--- list concat ---              33,556 copied / 470 reused (1.4% reuse)
--- attrs structural eq ---      8804 calls, 50,709 entries not cloned
--- M2 RISKY thunk-store ---     writes 448,864   loop_mutated 45,475   REDUNDANT 274,180
--- Storm A: referenced_idents --- walk_calls 152,150   nodes 4,003,289 (26.3/call)
  walltime:          236.2 ms   (1.8% of eval)
thunks_created: 1,273,696   thunks_forced: 613,726   (waste 51.8%)   max_force_depth: 73
```

**What the deep profile says, ranked:**
1. **The eval is apply/ident-bound, not storm-bound.** `ident` 33.8% + `apply`
   20.0% of 3.69 M expr evals. The dominant cost is the *volume* of thunk
   allocation + force + env-lookup, not any single O(N²)/re-flatten term.
2. **Thunk waste is the #1 closable term.** 1,273,696 thunks created, **51.8%
   never forced** — each pins a captured `Env` (72 B `EnvInner` + im_rc HAMT) that
   cannot be freed (no GC; `Rc` keeps it alive to end-of-eval). This is the
   biggest single memory driver (§3).
3. **Redundant thunk-stores are the #2 closable term.** 448,864 stores, **274,180
   (61%) provably-redundant rewrites** (`ThunkStoreRedundant`) + 45,475
   loop-mutated. PERF-ARSENAL's C-store already grades the *redundant* subset
   PROVABLY-NEUTRAL; the redundant-store skip is a shipped-but-partial lever.
4. **Storm A (1.8%) and Storm B overlay-flatten (2.1%) are BOTH small on the deep
   workload.** With Storm A now instrumented, the symmetric comparison is
   decisive: they are within a rounding error of each other and neither is the
   marquee. Overlay-flatten's cache hit-rate is 82.5% (builds/overlay = 0.14) —
   the cold-cache re-flatten storm the arsenal feared is **not** firing at scale.

### 2.3 Storm A is real but workload-shape-dependent (the symmetric reading)

With the new counter, Storm A's share of eval measured across three shapes:

| Workload | eval_expr | Storm A walltime | Storm A % | overlay-flatten % |
|---|---:|---:|---:|---:|
| `hello.drvPath` (shallow) | — | 178 ms | **4.7%** | (n/a small) |
| `evalModules` (2 modules, dyn key) | 4,629 | 2.2 ms | **18.4%** | ~0% |
| `nixosSystem` deep (baseModules) | 3,688,836 | 236 ms | **1.8%** | 2.1% |

The small `evalModules` slice over-weights Storm A (startup/parse-dominated,
10 ms total); the deep workload under-weights it (drowned by 3.69 M apply/force
evals). **Storm A is a genuine per-scope cost but is NOT the dominant eval term
on the marquee.** Fixing it would recover ≤1.8% of the deep eval — real, but not
the 4× lever.

## 3. Heap attribution — the biggest memory term (COUNTER-ESTIMATE, not heap-proven)

Tier: **counter-based byte-estimate** (sizeofs measured via a scratch `size_of`
probe on this binary; allocation counts from the perf counters). NOT a `dhat`
heap-proven attribution — that is a named follow-up (§7).

Measured sizeofs (release, this binary): `Value` 16 B · `Thunk` 8 B (an `Rc`) ·
**`ThunkInner` 72 B** · `ThunkRepr` 56 B · `Env` 8 B (an `Rc`) · **`EnvInner`
72 B** · `rnix::ast::Expr` 16 B.

Deep-workload allocation volume → estimated bytes:

| Term | count | ×unit (struct + Rc ctrl ~16 B) | est. bytes |
|---|---:|---:|---:|
| `ThunkInner` allocations | 1,273,696 | ×88 B | **≈ 112 MB** |
| `EnvInner` allocations (`env_clones`) | 658,289 | ×88 B | **≈ 58 MB** |
| retained im_rc HAMT node web | (per-binding, pinned) | — | **dominant remainder** |
| boxed `Concrete` cache values | ≤613,726 forced | ×(16 B + box) | tens of MB |

The two struct-allocation terms (~170 MB) are a **minority** of the +1784 MB
sui-over-nix delta. The **dominant memory term is the retained im_rc HAMT node
web + boxed cache values pinned by the 51.8% never-forced thunks** — Rust has no
GC, so every suspended-never-forced thunk holds its captured `Env`'s HAMT alive
until the whole eval completes. This is the classic lazy-evaluator retention
problem, and it is **the same root as thunk waste** (§2.2 term #2): the fix that
stops creating never-forced thunks *also* stops pinning their HAMTs. Attribution
of the exact HAMT-node byte total is the `dhat` follow-up.

## 4. The honest target + the closable / reprieve / inherent split

**HONEST TARGET: same order of magnitude (~1.5–2× of nix) on the deep workload,
and WIN on caching / correctness / the sui-owned typed flip — NOT "match nix."**
Matching nix's raw wall+mem needs `unsafe` arena allocation, a moving/tracing GC,
or hand-packed union representations that regress the safe-Rust thesis sui is
built on. The compounding win is a byte-exact, typed, Postgres-cacheable eval that
is *fast enough*, not a byte-for-byte perf clone of CppNix.

The gap splits three ways:

- **CLOSABLE — algorithmic + allocation storms (the M2 target).**
  - **Thunk waste (51.8%)** + its pinned-HAMT retention — stop minting
    never-forced thunks (demand-driven thunking / dead-binding widening). This is
    the single biggest lever on BOTH wall and memory. *Estimated headroom: large
    — it attacks the 20% apply + the memory delta at once.*
  - **Redundant thunk-stores (61%)** — the `ThunkStoreRedundant` skip (already
    PROVABLY-NEUTRAL for the redundant subset in PERF-ARSENAL C-store); land it.
  - **Storm A (≤1.8%)** + **overlay re-flatten (≤2.1%)** — real but small;
    byte-neutral to fix, low payoff on the marquee. Do them *last*, not first.
- **CONSTANT-FACTOR REPRIEVE (~1.3–1.8× memory).** The C-store double-store, the
  im_rc HAMT-node overhead vs a flat array, and the `Rc` control-block-per-thunk
  are constant-factor multipliers a representation change (e.g. bump-allocated
  Env chains, thunk-store collapse) can shave — but not eliminate.
- **INHERENT SAFE-RUST FLOOR (the irreducible part of the ~2×).** `Rc` refcount
  traffic, bounds checks, `OnceCell` cache cells, and im_rc-HAMT-node-vs-CppNix-
  flat-array are the price of memory-safe, GC-free evaluation. This is the part
  the honest target does NOT chase — chasing it regresses the safety thesis.

## 5. Caught bugs (filed)

- **(a) FLAKE-REF `nixpkgs#hello.drvPath` → `AttrNotFound("'hello'")`.** The
  bare-flake-installable attr-path form returns AttrNotFound (fast, not the ~36 s
  the task noted — the resolver rejects the attr immediately on this binary).
  `nix eval --raw 'nixpkgs#hello.drvPath'` returns the drv correctly, and sui's
  own **direct-import** form `(import <nixpkgs> {}).hello.drvPath` returns the
  byte-identical drv (§6). So the bug is isolated to **flake-installable
  attr-path resolution** (`<flakeref>#<attr>`), not eval. Reproducer:
  `sui --no-vm eval --raw 'nixpkgs#hello.drvPath'`.
  **Status: FILED here; not fixed (out of M0 scope — M0 is measurement).**
- **(b) PERF-ARSENAL §"overlay-flatten 21.5%" is an UNSOURCED artifact.**
  `PERF-ARSENAL.md:141-142` states the *"marquee cost is overlay-flatten 21.5% +
  force machinery"* as fact, with **no workload named and no counter cite** (grep:
  the figure appears exactly once, nowhere else). It **contradicts
  EVAL-PERF-SEAL.md §3**, which explicitly says *"The dominant storm — UNMEASURED"*
  and *"Profiler not yet run; dominance is a code-derived best-guess."* This M0's
  measured deep profile shows overlay-flatten = **2.1%** of eval, not 21.5%. The
  21.5% is a stale small-probe artifact stated with unearned confidence.
  **Fix: provenance-tag or delete it** (see the one-line correction queued in §7).

## 6. Byte-parity guard (the invariant this M0 must not break)

Instrumentation is measurement-only; it must change zero bytes.

| Check | Before (main, pristine binary) | After (instrumented binary) |
|---|---|---|
| `hello.drvPath` (`--no-vm`, direct-import) | `/nix/store/a1fzz00d2gwsj6kniyrivsyrdh97k634-hello-2.12.2.drv` | `/nix/store/a1fzz00d2gwsj6kniyrivsyrdh97k634-hello-2.12.2.drv` |
| nix oracle (same) | `/nix/store/a1fzz00d2gwsj6kniyrivsyrdh97k634-hello-2.12.2.drv` | (identical) |
| `sui parity` | 64 match | **64 match · 1 tracked · 0 regressions** |

Both hold. `hello` byte-identical to nix; `sui parity` unchanged at 64 match.

## 7. The M2 lever this profile chooses (the whole point of M0)

**The first real code lever is THUNK-WASTE + REDUNDANT-STORE, not Storm A.**

The measured, symmetric profile overturns the prior code-derived guess (Storm A
"strongest suspect"). On the marquee deep workload Storm A is ≤1.8% and overlay
re-flatten is ≤2.1%; the dominant, closable terms are:

1. **Kill never-forced thunks** (51.8% waste) — demand-driven thunking / widened
   dead-binding elimination. One lever that attacks BOTH the 20% apply wall cost
   AND the dominant memory term (the pinned-HAMT retention of never-forced
   thunks). **This is the M2 lever.** Byte-neutral by construction (a thunk never
   forced has no observable effect), still byte-verify against the nix oracle.
2. **Land the redundant thunk-store skip** (61% of stores; `ThunkStoreRedundant`
   already PROVABLY-NEUTRAL per PERF-ARSENAL C-store) — a smaller, already-proven
   lever, good as the M2 warm-up.
3. Storm A + overlay-flatten are **deferred to last** — instrumented, quantified,
   small. Fixing either is byte-neutral and cheap but recovers ≤2% each.

Named follow-ups (not M0): a `dhat` heap-proven attribution to convert the §3
byte-estimate to proven; wiring the deep-workload profile into `perf-seal`'s
corpus; and the two doc fixes in §5.

### Queued doc corrections (out of this M0's byte-neutral scope; noted, not applied)
- `PERF-ARSENAL.md:141-142`: change *"marquee cost is overlay-flatten 21.5%"* →
  provenance-tag as a stale small-probe artifact, superseded by this doc's
  measured **overlay-flatten 2.1% / Storm A 1.8%** on the deep workload.
- `EVAL-PERF-SEAL.md §3`: Storm A is no longer UNMEASURED — link this doc; the
  measured verdict is **A ≈ B ≈ small; the lever is thunk-waste**.

---

## 8. M2 — the byte-safe thunk-waste cut (LANDED) + creation-site attribution

> Worktree `m2/thunk-waste` off `main` (`sui parity` = 64 match). This M2
> **lands the byte-safe subset** of the thunk-waste lever §7 named, measures the
> creation-site attribution the M0 profile did not have, and **defers the RISKY
> laziness-observable subset with a named proof**. Tier: RELEASE build,
> tree-walker `--no-vm`, same deep marquee workload as §2.1.

### 8.1 WHERE the never-forced thunks are minted (measured, new)

M0 knew *how many* thunks were wasted (51.8%) but not *where*. M2 adds byte-neutral
creation-site attribution counters (`ThunkSite*`, gated on `perf::enabled()`,
symmetric with the M0 Storm-A counters) + a `maybe_thunk` `_`-arm expr-kind
histogram. On the deep workload, `thunks_created = 1,273,696` split:

| Creation site | count | % | byte-safe to reduce? |
|---|---:|---:|---|
| **native (deferred dotted-tail / `//` merge)** | 422,164 | 33.1% | **NO** — laziness-critical (M2.6 ROOT #2 defer; eliding = over-force → `null` in module system) |
| **maybe_thunk `_` arm** | 369,413 | 29.0% | PARTIAL — see §8.2 kind split |
| **apply lambda-arg** | 298,352 | 23.4% | PARTIAL — pure-constant subset only (§8.3) |
| rest (select-source / nested-attr) | 84,529 | 6.6% | NO (fixpoint) |
| recursive let/rec | 50,298 | 3.9% | NO (usually forced) |
| inherit-select | 45,029 | 3.5% | NO (usually forced) |
| with-ident deferred | 3,503 | 0.3% | NO (blackhole deferral) |
| maybe_thunk ident fallback | 408 | 0.0% | NO |

**Key correction to the M0 mental model:** the single largest thunk creator is
`native` (33%) — the **deferred dotted-path attrs + `//` merge** the module system
is dense with. These are the *most* never-forced, but eliding them is *categorically
byte-unsafe*: the deferral IS the M2.6 fixpoint-correctness fix (forcing `config` to
WHNF must NOT force a dynamic tail key). The expensive never-forced thunks are
exactly the ones whose laziness is load-bearing.

`maybe_thunk` `_`-arm by expr kind (369,413 total): Apply 133,455 · **Str 78,003** ·
Select 75,029 · Lambda(rec) 20,685 · BinOp 17,318 · List 12,032 · AttrSet 11,609 ·
IfElse 6,236 · With 3,902 · **Paren 3,557** · Assert 2,764 · LetIn 2,510 ·
Path(interp) 2,271 · UnaryOp 41 · HasAttr 1.

### 8.2 LANDED cut #1 — eager non-interpolated `Str` in `maybe_thunk`

A **constant string** (no `${…}` part) runs `eval_str` with zero force/coerce: it is
pure, non-throwing, side-effect-free, and yields `String(NixString::with_context(text,
EMPTY))`. Evaluating it directly in `maybe_thunk` is byte-identical to forcing a
suspended thunk of it. **Laziness-safety proof:** a value that cannot throw or diverge
is *inert* — a lambda/binding that never forces it observes no difference in eval
order; there is no `${…}` to force in the wrong env. Interpolated strings stay thunked.

Effect: `maybe_thunk` `_` arm 369,413 → 293,204; `thunks_created` 1,273,696 →
1,197,487 (**−76,209, −6.0%**).

### 8.3 LANDED cut #2 — eager pure-constant lambda-arg in `eval_apply`

Call-by-need thunks every lambda arg. The **pure-constant subset** (a literal, a
non-interpolated string, a non-interpolated abs/home path — `eval_pure_constant_arg`)
can never throw or diverge, so producing its value directly is byte-neutral whether
or not the lambda forces it. **Laziness-safety proof:** the classifier admits ONLY
inert constants and **rejects everything laziness-observable** — Ident (may force a
with-scope head, `lookup_fast` value.rs:2241), Select/Apply/BinOp/If/… (may throw),
interpolated Str/Path (must force `${…}` lazily). A `throw`-ing arg an ignoring lambda
drops stays fully thunked (test `ignored_throwing_arg_stays_lazy`).

Effect: apply lambda-arg 298,352 → 291,461 (−6,891). Cumulative `thunks_created`
1,197,487 → **1,190,596** (total from baseline **−83,100, −6.5%**).

### 8.4 DEFERRED (RISKY — named, with proof of why)

- **Full apply-arg elision (the Ident/Select/… bulk of the 298K).** RISKY: routing
  a general arg through `maybe_thunk`/`eval_expr` eagerly can force a with-scope head
  (`lookup_fast` forces the with-namespace attrset, value.rs:2241) or throw — a
  laziness-observable change if the lambda never uses the arg. Deferred until a
  per-arg "definitely-demanded-and-non-throwing" proof exists (a lambda body that
  provably forces its param first-thing).
- **`native` deferred dotted-tail / `//` merge (422K, the biggest bucket).** NOT a
  waste bug — the deferral is the M2.6 ROOT #2 over-force fix. Reducing it would
  reintroduce the `concatLists: expected list, got null` class. **Do not touch.**
- **Select / Apply / BinOp / If / With / AttrSet / Assert / LetIn `_`-arm thunks.**
  Each can throw, diverge, or observe a fixpoint blackhole (the Select comment in
  `maybe_thunk` documents the module-system dependence explicitly). Not byte-safe.
- **Paren (3,557).** Trivially safe (`(e)` ≡ `e`) but tiny; a recurse-through-paren
  is a follow-up, not this M2's marquee.

### 8.5 Measured effect (deep marquee, RELEASE `--no-vm`, best-of-N)

| Metric | baseline (main) | M2 | delta |
|---|---:|---:|---:|
| `thunks_created` | 1,273,696 | 1,190,596 | **−83,100 (−6.5%)** |
| maxRSS | 1974.8 MB | 1970.4 MB | **−4.4 MB (−0.22%)** |
| wall (best-of) | 10.56 s | 10.44 s | −1.1% (≈ noise) |
| result | `"nixos"` | `"nixos"` | byte-identical |

The RSS win is honest-small: the byte-safe cuts are the *cheap* thunks (pure
constants capture a trivial env, no retained HAMT). The dominant memory term —
retained HAMTs of never-forced thunks — lives in the RISKY `native`/Select buckets
whose laziness is load-bearing, so it is **not** reachable byte-safely. The larger
wall/mem lever remains a representation change (bump-allocated Env chains / thunk-store
collapse — the §4 constant-factor reprieve), not more elision.

### 8.6 Byte-parity guard

| Check | result |
|---|---|
| `hello.drvPath` (`--no-vm`, direct-import) | `a1fzz00d…-hello-2.12.2.drv` — byte-identical to nix |
| `sui parity` | **64 match · 1 tracked · 0 regressions** (unchanged) |
| neovim (the `1 tracked`) drv hash | `8n1325kk…` — byte-identical between main and M2 (change did not perturb the tracked divergence) |
| darwin `currentSystem` probes (hello/stdenv/bash/coreutils/openssl/curl) | all match |
| `sui-eval` lib tests | 1369 passed (+3 new) · 1 pre-existing failure (`evaluate_flake_fetch_failure_returns_error`, fails on pristine main too) |

New regression tests pin the invariant: `maybe_thunk_eager_constant_str_is_byte_identical`,
`eval_pure_constant_arg_classification`, `ignored_throwing_arg_stays_lazy`.
