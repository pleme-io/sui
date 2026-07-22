# DARK-SIDE OPTIMIZATION for sui — design + reusable artifacts

**Provenance (tier-honest).** This is the synthesis of a 7-agent research pass
(internal sui reality + cppnix/tvix env models + Rust arena craft + academia/math
+ interpreter techniques + correctness-gating), **confirmed by** an independent
symbolicated CPU profile of the tree-walker on an env-churn workload
([`DARK-SIDE-PROFILE.md`](./DARK-SIDE-PROFILE.md)) and **reconciled with** the
prior measured `ENV-RESOLVE-DESIGN.md` work. The correctness boundary it operates
inside is [`STRATOSPHERE.md §8`](./STRATOSPHERE.md). Nothing here is shipped except
where a row says so; a `Result::Err` is mitigation, a compile error / absent path
is unrepresentability — never rounded up.

Research axes cited **[A1]**…**[A6]**. The two most load-bearing claims —
(a) "copy cppnix's flat frames" is a **proven net-negative trap** without a
GC/arena, and (b) the 7× gap has **two roughly-equal shared roots** (env-alloc
churn + rowan AST re-walk), so `eval_ir` and env-capture-shrink are co-top levers —
are grounded in the ledger's own DISCARDED rows and the dhat/profile shares, not
inferred.

---

## 1. The problem, precisely stated

**A dark-side optimization is a performance change that is NOT
observable-equivalent by construction** — it can perturb the answer under some
demand order, resolution path, or partial-value shape — **and is therefore only
*conditionally* correct, its correctness resting on an external byte-oracle over a
partial corpus rather than a structural guarantee.** The "light side" changes the
representation but the denotation provably cannot; the dark side buys speed against
a *risk of a silently wrong answer*. On the dark side **a green build is not
proof** — the worst failure is a *different-but-plausible* `.drv` hash no test
covered.

**The correctness boundary** (`STRATOSPHERE.md §8`): a **total external oracle
(cppnix) over a partial corpus** (`sui parity` 77 + `lang` 116 + `build-parity`).
sui has no test-oracle problem — the right answer is `nix eval` — which collapses
the method to straight **differential testing**. The whole-closure NAR differential
is still DESIGN (M2), so an opt that preserves eval-artifact bytes but perturbs
realized store contents is currently **ungated** — a named hole. Two ceilings never
rounded past: `PartialCorpus` and `ExternalObservation` (C2, forever).

**Why honesty is the *enabling* constraint** — `perf.rs` already makes two failure
classes unrepresentable on the type axis: `Delta::measured(before,after) → None`
when `after ≥ before` (a shipped regression is unrepresentable on the sign axis),
and `claimed_tier ≤ earned_tier(technique)` (a resolution change claiming
`ByteSufficient` is a caught `TierOverclaim`). Because a wrong optimization
*cannot be marked promoted, cannot lie about its speedup, cannot lie about its
risk tier*, the operator can push full-throttle on prototyping without a mislabeled
fast path silently reaching a user. **The ledger is the welded catch; full-throttle
experimentation is the floored recursion the catch makes safe.**

---

## 2. The ranked lever list

The 7× user-CPU gap on `hello.drvPath` (**both** sui engines equally slow ⇒ the
cost is the shared substrate) has two roughly-equal roots: **env-alloc churn**
(env COW-per-bind = 42.7% churn / **80.8% live peak**) and **rowan AST re-walk**
(40.7% bytes / 21% wall) **[A1]**. The cid marquee **DNF is a *memory* death**
(retained captured-Env HAMT chains, 73 GB) — memory levers matter as much as CPU.

> **The naive "copy cppnix's parent-pointer flat frames, drop the HAMT" is a TRAP
> sui already sprang and rejected.** `positional-frames` measured **net-negative
> (+7% fib / +32–39% call-heavy)** — the frame *allocation* costs more than the
> HAMT probe it removes. cppnix's flat frames win **only because of its Boehm GC**
> (`allocEnv` = pointer-bump, zero refcount traffic) **[A2][A4]**. **Frames win in
> sui ONLY coupled with region/arena allocation** (Tofte-Talpin per-eval region,
> bulk-free at eval end). So the top lever is the *coupled* move, decomposed into
> individually-tractable sub-levers ranked below — never the monolith.

| # | Lever | Attacks | Byte-risk | Tractability | Gate |
|---|---|---|---|---|---|
| **1** | **`eval_ir` full-engine wiring** — lower rowan→flat `ExprId` arena once/file, eval the arena, `ProgramCache` amortizes | rowan re-walk (40.7% bytes) | **RISKY** — walker re-implemented vs mirror `IrValue/IrEnv`; byte claim rests on the differential | **Highest — ALREADY BUILT, wired into nothing**; measured **2.58× micro WARM**; `eval_differential.rs` exists | dual-engine differential over corpus+lang+build+proptest; per-builtin rows for 36 natives. **WARM only; cold ≈ neutral (parse+lower ≈ the rewalk saved).** |
| **2** | **env-capture-shrink** — narrow each `Suspended{env}`/`Closure{env}` to capture only the free vars its body reaches | env COW (42.7%) + **the cid-DNF 80.8% peak** | **RISKY** (`DropUnobservedOrder`) — a missed name (`with`/`${}`/`inherit(from)`) → spurious `UndefinedVar` | **High — half-built:** free-var set exists (`referenced_idents`, `eval.rs:755`), parked as "L7" in the ledger | must **over-approximate** (blanket-keep `with`-scopes + every dynamic channel); differential + **peak-RSS on the marquee** (the memory ratchet). Keeps the HAMT → dodges the frame trap. **Attacks the actual DNF.** |
| **3** | **batch-bind** — collapse N `Rc::make_mut`+`insert` COWs in `bind_param` (`eval.rs:3374`) into one `make_mut` + N inserts | env COW (42.7%) | **SAFE** (`SkipRedundantStore`) — same final HAMT, fewer path-copies | **Highest ease, ~1 day** — HAMT-preserving, dodges `PersistentLazyDesign` | interleaved A/B + one confirming byte-check. **Ideal harness warm-up.** |
| **4** | **per-eval arena/index alloc of thunks & frames** (bumpalo/id-arena) — kill per-thunk `malloc/free` + refcount | `Rc<ThunkInner>` churn (7.6% peak) + the frame-alloc cost that sank `positional-frames` | **RISKY** — changes finalization/lifetime; strictly single-threaded; escaping thunks (eval_cache/graph-store) must not hold a reset index | **Medium — the enabling half of the top coupled lever.** `sui-ir` already ships the index-arena AST pattern | `SUI_LIVE_CENSUS` on during rollout (peak must DROP); double-run byte verify-mode; Miri clean; never let arena objects outlive their eval. |
| **5** | **de-Bruijn `{up,slot}` resolution** (deferred `sui-resolve` M1) — array index for the HAMT probe | env lookup + churn | **RISKY**, pays **only coupled with #4** — alone this IS `positional-frames`, **REJECTED** | **Medium — M0 half shipped** (`Resolution::Lexical{sym}` + `WithBarrier` under `SUI_RESOLVE=1`) | reuse `sui-resolve` parity-by-construction (fails safe to `Dynamic`); **do NOT ship without #4** (the proven trap). |
| **6** | **NaN-box the `Value` word** (~16B→8B) | per-Value bandwidth (every clone) | **RISKY** — unsafe transmute, provenance, manual Rc inc/dec | **Medium — prototyped** (`sui-bytecode` `NanBox`); Lua ~20% | Miri/ASAN + safe-enum reference oracle, differential both; round-trip proptest for int/float/ptr fidelity. |
| **7** | **thunk elision / strictness** — eagerly eval provably-forced positions | thunk-alloc waste (**51.8% never forced**) | **RISKY** — eager eval of a would-throw/diverge expr is byte-observable | **Medium — SAFE subset shipped** (`maybe_thunk`); byte-safe eager fraction ≈ 0% per ledger | restrict to provably-total/pure operands; one-way latch; ledger already discarded the general form. |
| **8** | **CHAMP-compact HAMT** (Steindorfer-Vinju) — if the HAMT stays, compact the node | HAMT cache-hostility | **SAFE** — layout only | **Low-medium** — replaces `im_rc` node | byte-neutral by construction; complements #2. |
| **9** | **value interning / singletons** — small ints, common strings, empty list/attrs | alloc + O(1) ptr-eq | **SAFE** — identity not content | **Low-medium** — idents already interned | verify list/attrs Rc identity feeding `==`/context. |
| **10** | **superinstructions** on the Ir arena — fuse `Ident→Force`, `Select→Apply` | dispatch count | **SAFE** — dispatch rewrite | **Low** — best after #1 | differential only. |

**Explicitly OUT** (ledger-proven traps): `positional-frames` alone (net-negative);
swapping Env to a persistent-vs-std map without frames (`overlay-merge-structural`:
5–11× merge / 1.1–3.4× lookup double-loss — Env *needs* the HAMT for `child()`'s
O(1) share, so the win is capture-*shrinking* the HAMT, not replacing it); general
`thunk-waste-elision`/`dead-binding-elim` (byte-safe eager fraction ≈ 0%).
Deferred: copy-and-patch JIT (huge codegen surface, after #1–#4 saturate);
PICs (Nix attrsets have no hidden-class/shape). **No ML** — every technique is
top-of-the-classical-ladder (de Bruijn, region calculus, closure conversion, CHAMP,
strictness analysis, NaN-boxing). Brilliance, never a black box.

---

## 3. The typed vocabulary — `/vocabularify`

Code: `sui-spec/src/darkside.rs` + `sui-spec/specs/darkside.lisp`. **Extends**
`perf.rs` (Operating Principle #1 — extend the near-miss, don't fork), adding the
byte-risk axis, the gating-method axis, and a **promotion typestate ladder whose
top rung (`Promoted`) is unconstructable without an evidence witness** — so
"promoted a wrong answer" / "promoted without a backstop" / "promoted without a
named ceiling" are *unrepresentable*, not merely caught. The `(defdarkside-lever …)`
authoring surface mirrors `(defperf-lever …)`; catalog reflection tests every row
so a status can't overclaim. See the code for the full border; the load-bearing
type is:

```
PromotionStatus::Promoted { verified: Box<Verified>, backstop: NamedBackstop, ceiling: Ceiling }
```

A `ByteRisky` lever can never reach `Promoted` without a `DifferentialOracle` gate
(honesty-caught); a `Resolution`/`ForceOrder`/`PartialShape` change claiming
`ByteSafe` is a `TierOverclaim`. This is how the problem is *captured + testably
moved forward*: adding a lever is *write code + write the row + the row can't lie*.

---

## 4. The reusable method — `/dark-side-optimization` skill

The six-rung promotion ladder (`dark → default`):

```
0 CLASSIFY       byte-SAFE (Representation/RedundantWrite) → one byte-check.
                 byte-RISKY (ForceOrder/Resolution/PartialShape/Lifetime) → full ladder.
1 RESEARCH+BOUNDARY   ground the cost in the dhat/perf ledger (never a guess);
                 confirm the oracle covers it; name the ceiling it will still carry.
2 PROTOTYPE OPT-IN    behind a SUI_* flag, OFF by default, zero-cost unset;
                 a one-way latch (reject-to-slow-path, never clamp).
3 MEASURE HONESTLY    deterministic op-count/perf-seal for speed; wall report-only;
                 LOAD-ROBUST user-CPU never wall; refuse a null + a regression;
                 memory levers measure peak-RSS on the marquee.
4 DIFFERENTIAL-GATE   run BOTH paths; diff observable bytes over corpus+lang+build
                 (+ whole-closure NAR once M2 lands); risky path stays dark on ANY diff.
5 PROMOTE-ON-PROOF    default-ON only when corpus-green across the risky path with
                 zero regressions + delta>0 honestly-tiered + a bounded backstop
                 for uncovered demand orders. Keep the flag as a kill-switch. Record the ceiling.
6 SEAL           the divergence becomes a permanent red gate; a KnownDiverge
                 auto-graduates the moment parity lands. (→ /algorithmic-prowess-seal)
```

Anti-patterns (each a caught `honesty_violation` or a ledger-proven trap):
null-as-win, tier-overclaim, copy-cppnix-frames-alone, wall-time headline,
green-as-proof, metamorphic-as-primary-gate, clamp-on-mismatch, promote-without-a-backstop.

---

## 5. The `/algorithmic-prowess-seal` contribution — "Optimization Sealing"

Dark-side optimization is the **performance instance** of APS's core move: turn an
invariant into a type so its violation is unrepresentable. The invariant is
**byte-neutrality**. Seal per byte-risk tier:

- **truly-unrepresentable** — `ByteSafe{Representation}` whose type guarantees
  identical observation (the `Concrete`/`demand()` split: `demand()→Concrete` has
  no `Thunk` variant, so "observed a thunk" is a compile error). No oracle needed.
- **construction-time-rejected** — the **promotion typestate**: `Promoted`
  unconstructable without `Verified`+`NamedBackstop`+`Ceiling`. "Shipped an
  unproven optimization as default" has no code path. *(new seal this adds.)*
- **CI-caught (C2)** — a `ByteRisky` lever's differential corpus is a permanent red
  gate; a `KnownDiverge` auto-graduates when parity lands. The honest floor, labeled.

Add to the seal skill: (1) the `Delta` positive-only seal (regression
unrepresentable on the sign axis) as the canonical example; (2) the
promotion-typestate seal as a reusable pattern; (3) the no-ML boundary stated
(dark = risky-to-correctness, never opaque); (4) seal-and-route (the honesty
ledger IS the deterministic tool replacing agent judgment about "is this safe to
ship"); (5) *a seal is honest only when its ceiling is named*.

---

## 6. The first experiment (M0) — run NOW

**Two-track:** a byte-SAFE warm-up that proves the harness+ledger end-to-end, then
the headline dark-side run against an already-built, already-measured lever.

**M0-warm-up (byte-SAFE, ~1 day) — `batch-bind`.** `bind_param` (`eval.rs:3374`)
does N `Rc::make_mut`+`im_rc::insert` COWs for an N-formal pattern lambda. Collapse
to one `make_mut` + N inserts on the owned map. `ByteSafe{RedundantWrite}` (identical
final HAMT, HAMT preserved → dodges the frame trap). Flag `SUI_BATCH_BIND`; measure
via `perf-seal` op-count + `marquee_perf_profile.rs`; one confirming `sui parity`.
Proves the ledger flow + banks a real churn win + de-risks the harness.

**M0-headline (byte-RISKY) — wire `eval_ir` behind `SUI_IR` (subset first).** The
only top lever with an existing impl **and** a measured number **and** a differential
harness. Wire the pure arith/let/lambda/select subset through the engine behind
`SUI_IR=1` (OFF by default, tree-walker fallback on any uncovered node). NOT full
`ir-file-eval` yet (36 natives + `ProgramCache` mutation probe = M0.5).
`ByteRisky{Representation}` — the walker is re-implemented vs mirror `IrValue`, so
the byte claim rests on the differential. Extend `eval_differential.rs` to run
`eval_ir` vs tree-walker (semantic oracle) vs `nix` over `parity`+`lang`+`build`,
shrink-only `KnownDiverge` allowlist, any diff → stays dark. **Claim WARM only;
cold ≈ neutral — recorded, not rounded.** Promotion is NOT this session.

**M0-headline — LANDED (shadow, 2026-07-22).** `SUI_IR` now wires `eval_ir` as a
**live shadow engine** in `sui eval` (`src/main.rs`): with the flag set, `eval_ir`
runs alongside the tree-walker and reports agreement on stderr
(`[SUI_IR shadow] MATCH | DIVERGE | eval_ir GAP (…)`), while the **tree-walker stays
authoritative — its bytes are what ship**, so the shadow can never emit a wrong
answer (proven: stdout byte-identical with/without the flag; an uncovered node
reports a typed `Unsupported(…)` gap and the walker's answer ships). The
prerequisite renderer was promoted from test-only into shippable code —
`sui_ir::render::render_ir_value` + `sui_eval::render::render_tree` — with the
existing `eval_differential` (13/13) proving the lift byte-neutral. This realizes
`DarkGated` (rungs 2+4): `eval_ir` is exercised on every real `sui eval` toward
promotion; **speed is NOT yet realized** (both engines run) — that awaits promotion
to `eval_ir`-authoritative on a whole-corpus parity proof (NOT this session).

**M1 (named, not this session):** `env-capture-shrink` behind `SUI_CAPTURE_SHRINK`
— reuse `referenced_idents`, over-approximate, gate on full parity **AND** peak-RSS
on the cid marquee. The lever that actually attacks the DNF; inherits the M0 harness.
