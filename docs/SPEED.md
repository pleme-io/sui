# SPEED — the fastest possible sui

> **Status:** STRATEGY (canonical). This document merges the two greenlit
> designs — the **runtime-architecture answer** (what the evaluator becomes)
> and the **staged-pragmatism schedule** (the dependency-ordered path) — with
> the 2026-07-21 **adversarial verification pass folded in as overriding
> corrections**: where a design and a correction conflict, the correction
> wins, and the divergence is named inline rather than smoothed over.
> Tier labels are honest — **MEASURED** (a recorded number with committed
> provenance) / **PROJECTED** (derived from a measured share, wall unproven) /
> **HYPOTHESIS** (mechanism plausible, no number) / **STRUCK** (a number
> quoted with no committed provenance — inadmissible until landed).
> Never round up. Companions: [`benches/USE-CASE-MATRIX.md`] (the claim's
> falsifiable form — §IV here is its condensed canonical statement),
> [`sui-spec/specs/perf.lisp`] (the typed lever ledger — every landing ships
> a row), [`EVAL-MEMORY.md`], [`PERF-CLAIM-LEDGER.md`], [`SUI-EQUIVALENCE.md`].
> **Known-stale sibling:** [`PERF-ARSENAL.md`]:51 still claims
> `AttrsInner::Flat` is an im_rc HAMT — superseded by `fab4566` + `582cede`
> (§II). Source beats docs; that line is the worked lesson of invariant I9.

---

## §0 The destination (unhedged)

The fastest possible sui is **not** a tvix port, **not** a Boehm-GC
transplant, **not** a Perceus rewrite, and **not** a generator/CPS machine.
It is the current parity-sacred tree-walker — the **only** parity engine
(`--no-vm`; the VM defers string-context and is not parity-capable) — carried
through four structural surgeries plus an orchestration shell, in dependency
order:

1. **Lower-once flat IR.** rowan CST → arena `Program`/`ExprId(u32)`,
   lowered once per source **content hash**, evaluated forever after. Deletes
   the largest measured CPU/alloc class on the books (40.7% heap bytes /
   51% alloc calls / 21% wall) and makes the twice-proven
   `(source_id, offset)` cache-unsoundness class *unrepresentable* — there is
   no global node id to confuse across sources.
2. **The retention bundle under the no-GC ceiling.** Columnar shape-table
   attrsets (re-based against the heap that exists **now**, not the one dhat
   profiled in July's first week) · trimmed thunk capture (census-gated) ·
   tail-**Apply** release (the rest of the tail trampoline already ships) ·
   layered `//` chains (A/B'd against the shipped flatten-then-release
   baseline) · CA-spill of write-once payloads. Each lever's magnitude is a
   measurement, not an inheritance.
3. **Orchestration, not engine mutation.** Process sharding with
   footprint-threshold worker restart + the shipped 3-tier eval cache
   (407× MEASURED). The operating reframe for the marquee: **U10 must
   complete once, bounded, inside the machine; every eval after is a 0.03 s
   cache hit.** Seed once, serve forever.
4. **Two held ceiling-crossings, evidence-gated.** The conservative
   tracing-GC fork (its CPU price is currently *unmeasured* — it must be
   measured before it can be priced, §III L11) and the warm-daemon/salsa
   incremental rebuilder (U11 — the class no Nix evaluator anywhere ships).

Success is the matrix, never a headline: **U10 flips CANNOT-COMPLETE → G0
inside the row ceiling; U05 stays G1 (its ~9.4× gap is the priced ceiling of
the persistent-lazy no-GC design); no green row regresses.** Parity 77/77
precedes every recorded number. A fast wrong answer is not a row.

---

## §I Standing invariants (bind every phase, every lever)

1. **Parity precondition, not gate.** `sui parity` 77/77 green on the
   tree-walker before any perf number is recorded.
2. **Never land neutral on the sacred path** (apply-trace-clone precedent).
   Wall claims only from interleaved A/B, release profile only.
3. **Footprint, not RSS.** The U10 death mode is retained-allocation slope
   with a small resident fraction; `max_rss` grades a dying eval as healthy.
   Memory rows sample phys_footprint and record both.
4. **Every landing ships its `defperf-lever` row** in `perf.lisp` in the same
   commit; unproven wall stays `measured: Pending`.
5. **Technique-class honesty.** ReprSwap / MemoizeIdempotentQuery →
   ByteSufficient (corpus proves it); ForceOrderChange → force-order proof;
   ResolutionChange → Rejected by default (attrset-symcache precedent).
6. **A profile share is not a wall number.** `share` and `wall` are separate
   fields; only `wall` feeds G1/G2 (canonicalize-memo discipline).
7. **The fence is law** (§III.E). A red row's remediation may not re-attempt
   a lever the ledger Discarded with a named ceiling.
8. **Standing zero-perf riders:** BString-class byte-string NixString
   (closes the latent non-UTF8 parity class before corpus growth hits it);
   never implement `Eq`/`Hash` on force-capable `Value` (tvix's
   abandoned-design verdict).
9. **Source beats docs; the ledger beats memory** *(new — the verification
   pass's own meta-finding)*. Both merged designs inherited a stale doc line
   ([`PERF-ARSENAL.md`]:51) and sized a flagship lever against a heap that
   had already been replaced on main. Therefore: (a) every magnitude claim
   names the **commit** of the profile it is sized against, and is re-run if
   the named surface churned since; (b) a number with no committed
   provenance is **STRUCK** — inadmissible for sizing or gating until the
   run that produced it lands in the baseline. Struck by this rule as of
   this writing: the 64 GB/7 GB U10 pair, the ~29% GC-price figure, the
   387M-slot/6.2 GB `//` datum.

---

## §II Corrected ground — what is true on main today (@ `582cede`)

The verification pass re-read the source; these facts **override** both
designs' premises wherever they conflict.

- **Attrsets are already flat hashbrown.** `fab4566` (2026-07-15, 93 minutes
  *after* the dhat profile `0b35520`) converted `AttrsInner::Flat` from the
  im_rc HAMT to a std hashbrown `AttrsMap<Symbol, Value>`; `582cede`
  (2026-07-21) killed sym-keyed iteration as the measured #1 CPU sink;
  the Overlay flatten-then-release also shipped (`value.rs:2270-2284`).
  **The dhat split 45.2/29.7/17.5/7.0 describes a heap that no longer
  exists.** An unmeasured part of the old "HAMT→flat roughly halves the
  45.2%" projection is already banked; post-flip, the env im_rc share is
  presumably relatively larger. A **fresh dhat is a hard precondition**
  before any U10-slope delta is attributed to further attrs work.
- **SipHash was never on the attr lookup path.** Even pre-`fab4566`, the map
  used `FxBuildHasher` (`value.rs:22-25`: "no SipHash overhead" for
  `Symbol(u32)` keys). The only SipHash on the named surfaces is
  `compute_needed_bindings` / `collect_referenced_names`' String-keyed std
  maps (`eval.rs:8, 418-444`) — exactly what lever L1 targets. The honest
  residual attr-path claim for columnar+Shape is **Fx multiply-shift probe →
  indexed load / binary search** — a much smaller delta than any
  "SipHash-removal" framing implies.
- **The tail trampoline already ships.** `eval_expr_inner` loops in place
  for tail-position if/else, let..in, with, assert, paren, and root
  (`eval.rs` ~1206), rebinding `cur_env` and dropping the caller's env Rc
  early for all those forms today. **Only `Apply` exits the loop.** The
  genuinely novel tail-release scope is tail-Apply only (L8).
- **The ident cache is already hardened.** `8bad3ef` keyed it on the env
  (not an unmaintained thread-local); `b35b74d` restored `source_id` on
  thunk force (cross-file intern collision); `EnvInner.source_id`
  (`value.rs:2585-2591`) carries the defining file across lazy forces. The
  symcache re-open (L4) is *not* "sound for the first time" — it is the
  cleaner structural close, and its +25.4% Deferred-era number must be
  re-based against this hardened baseline.
- **The eval cache already has per-attr keys.** It keys
  `sha256(desugared-expr ⊕ render-mode) ⊕ sha256(flake.lock)` — "a
  different attr / mode / lock can't collide" — with
  `SUI_EVAL_CACHE_VERIFY=1` as an anti-stale differential and
  installable-only admission (bare `--expr` never cached). The genuinely-new
  cache work is the **dirty-tree hashing policy** only. There is **no**
  "stale-serve incident" in the sui record; the cache shipped *with* its
  anti-stale protections. (Nearest real cache-poisoning precedent: rio's
  attic non-reproducible artifacts, 2026-06-02 — a different system;
  citable as motivation for hash-never-ignore, not as a sui incident.)
- **The thunk-pin attribution is unreconciled.** The allocation-context dhat
  labeled slice B (29.7%) "under thunk-capture call context"; the
  thunk-env-repr live-peak dhat attributed the dominant live HAMT term
  (80.8%) to `Env::bind` under `bind_param ← apply`. Thunk capture itself
  allocates no HAMT nodes — `Suspended{expr, env}` (`value.rs:1147-1150`)
  takes the env by Rc bump; the pinned chunks were allocated at bind sites.
  **What trimmed capture (L7) actually frees is the env chunks pinned
  *solely* by never-forced thunks** — an unmeasured fraction of the old
  29.7%, not the slice "directly". The pinned-bytes census is the honest
  gate.
- **`value.rs:1249-1262` is the thunk state machine** (OnceCell cache +
  UnsafeCell repr + `recursive: bool`), not rec-env construction — it
  evidences nothing about capture order. If anything it cuts the other way:
  the `recursive → Promise` mechanism is direct proof that mid-construction
  observation is a real, designed-for state in rec scopes. **The L7
  rec-scope spike's burden is raised, not lowered.**
- **`get_sym`'s doc-comment is stale** (`value.rs:2313-2325` describes a
  lazy right-to-left walk the code no longer does; the code routes through
  `as_flat`, `value.rs:2327-2336`). The surface churned recently — any
  layered-`//` design (L9) re-reads it at build time.
- **Restart prior art is restart-plus-GC.** nix-eval-jobs restarts workers
  on a memory threshold, but those workers run cppnix **with the Boehm
  conservative GC active underneath**. There is no prior art for a GC-free
  Rc evaluator surviving on restarts alone. The sharding design (L12) stays
  sound; the "restart, not GC" framing is dropped and does not weigh
  against the GC fork (L11).
- **U10 verified numbers:** 20,827 drvs ([`DARWIN-PARITY-CAMPAIGN.md`]:62);
  nix ~107 s ([`SUI-EQUIVALENCE.md`]:294); sui ~140 MB/s linear retained
  slope (`perf.lisp` sym-keyed row); freshest **recorded** sui footprint
  figure: **25 GB** (canonicalize-memo record, 2026-07-21); EVAL-MEMORY
  instrumented peaks 14.6/18.8/~22 GB with nix at 10.68 GB on the identical
  instrumented eval. The 64 GB/7 GB/11%-resident triplet is off-ledger —
  **STRUCK** until a recorded run lands in the matrix baseline (S0).
- **Still true, inherited whole:** 51.8% of thunks are never forced; the
  in-house live census (`fc30a62`) found Rc reclaiming aggressively and
  **zero uncollected cycles** (Bacon-Rajan stays unscheduled); the CPU fence
  on overlay-flatten (≤2.1% real-eval) and `//`-merge-structural
  (Discarded) holds; cppnix is literally Boehm-conservative-GC'd; cppnix
  #13987's layered-attrs "~20% less memory" was measured against **its**
  eager-copy-on-`//` baseline, not against sui's shipped
  flatten-then-release.

---

## §III The levers (architecture, corrected)

### A. Measurement substrate

**L0 — Re-ground: fresh dhat + footprint checkpoints + the matrix
baseline.** Run the cid marquee on current main under dhat and a
**footprint-at-work-checkpoints** sampler (phys_footprint sampled at fixed
deterministic EvalExpr counter checkpoints, e.g. every 50M — the slope
series is build-comparable even when neither run completes; this is U10's G1
metric until G0 flips, and the only honest way to A/B L6/L7/L9/L10). Land
[`benches/USE-CASE-MATRIX.md`] + `use_case_matrix.rs` + mint
`benches/use-case-baseline.json`, replacing every STRUCK number with a
recorded one. **Nothing memory-side lands before this exists.**

### B. CPU axis

**L1 — ~~Fx/Symbol rec-scope analysis~~ CORRECTED (2026-07-21, I9 in
action): the target is DEAD CODE.** `compute_needed_bindings` /
`collect_referenced_names` (`eval.rs:395-451`) have **no caller** — `cargo
check -p sui-eval` warns `never used` for both, and `git log -S` shows they
were born unwired in `84da28c` (2026-04-12, "dead binding elimination
*infrastructure*") and never connected in three months. Converting their
SipHash maps to FxHash would optimize code that never executes; the
"one genuine SipHash kill on the books" claim is void.

The finding worth more than the defect: **the infrastructure that would
attack the measured 51.8%-never-forced-thunk root already exists and was
never turned on.** Wiring dead-binding elimination = creating fewer
never-forced thunks = fewer pinned Envs — the retention wall's named
driver, at its creation site. That is a *different, bigger, riskier* lever
than L1-as-written: it changes which thunks are ever *created*, so it is
gated on the **S1 rec-scope spike's** correctness verdict (dynamic
reachability: `rec` attrsets are attr-enumerable via `builtins.attrNames`;
plain `let` bindings are not — the elimination is plausibly `let`-only at
first). L1 is hereby folded INTO the S1 spike as its motivating payload;
the FxHash conversion happens, if at all, as a detail of wiring it.

(How this survived three review passes: every check verified the code
*exists* at the cited lines — none asked whether it *runs*. Liveness is now
part of I9's checklist: a lever's target must be shown reachable, not just
present.)

**L2 — Path intern/memo** on the ~5% `std::path::Components` share.
MemoizeIdempotentQuery, canonicalize-memo's sibling, frozen-inputs
assumption named. Memo on path-string content only — explicitly **not**
per-node cached until L3 provides stable identity.

**L3 — Lower-once flat IR** (the ledger's own named next lever; the largest
CPU lever in this document). `sui-ir`: per source file one
`Program { exprs: Vec<Ir>, spans, aux }` with `ExprId(u32)`, lowered from
rowan once, keyed by **content hash only** (never path, never mtime — the
key discipline the eval cache proved). Lowering precomputes: interned
ident/select-path Symbols (extends select-ident-token +12.6% to the class) ·
string literal parts · normalized path literals · per-rec-scope
needed-bindings/referenced-idents (retires L1's per-eval recompute) ·
free-var sets (feeds L7) · attrset-literal shapes (feeds L6). Lowering must
**not** pre-resolve idents to slots/frames/levels (m0-resolver NULL;
positional-frames net-negative; ResolutionChange earns Rejected). Phase-1
lowering is 1:1 structural — force order untouched by construction;
superinstruction fusion is a later, per-candidate force-order-argument
phase, dispatch-only, never eagerness. Migration: dual-engine
(`--ir` behind a flag, full corpus byte-diffed both engines, hot-shapes
interleaved A/B) → flip default → rowan retired behind a flag ≥1 release
(MODULARIZE, DON'T DELETE — it stays the tie-breaker oracle). Thunks switch
to `(Rc<Program>, ExprId)` capture as a **separately-gated follow-up
commit** (it changes what `Suspended` holds; also removes the rnix heap
share's per-thunk term). Class: ReprSwap → ByteSufficient.

**L4 — Symcache re-open, per-(Program, ExprId)** (after L3). The +25.4%
Deferred lever, structurally closed by per-Program ids. Corrected framing:
main already hardened the ident-cache class (`8bad3ef`, `b35b74d`, §II) —
this is the cleaner close, **not** the first sound one, and the re-open
A/B diffs against the hardened IDENT_CACHE baseline, not the Deferred-era
one.

**L5 — Inline caches at Select/HasAttr sites** (after L6). One-entry
`(shape_ptr, value_index)` per ExprId site; monomorphic hit = ptr-compare +
indexed load. Soundness rests on shape interning + ExprId per-Program
scoping — the exact two preconditions whose absence sank both prior cache
attempts. Expected honestly low (select text materialization already
harvested); measure-first, land on Improved only.

### C. Memory axis — the U10 campaign

**L6 — Columnar + shape-interned attrsets.** The twin-reasoning survivor,
**re-based**: the starting point is the *current* hashbrown
`AttrsMap<Symbol, Value>` (§II), not the im_rc HAMT.
`AttrsInner::Flat → { shape: Rc<Shape>, values: Rc<[Value]> }`, `Shape`
globally interned by key-set; literal-keyed attrsets (the mkDerivation
millions) share one shape — per-instance key storage and per-instance hash
tables disappear; dynamic-keyed sets fall back to a private shape (still
columnar). The honest CPU claim is **Fx probe → indexed load** (small); the
prize is the retained-bytes structure. `Overlay` and the flatten cache stay.
**Open decision D1 (must be resolved before build; the design as previously
written silently re-walked a killed path):** the shape's key order.
The order-preserving interner (u32 order ≡ nix byte-sort) was **killed** by
twin-reasoning (collation silent-break; renumber invalidates live Symbols;
re-solves the shipped drop-unobserved-order) and stays killed. The two
honest options: **(a)** keys stored byte-sorted → the shape-miss path is
string-compare binary search, or a u32-rank side permutation per shape
(neither free); **(b)** keys stored u32-sorted → `sorted_entries` still
pays resolve + lexicographic sort (it is **not** free). Pick per measured
mix of miss-path lookups vs `sorted_entries` calls on the fresh profile.
Parity gates inherited verbatim: key-collation byte-parity beyond the
corpus (non-ASCII + `__` keys, nix byte-sort — fuzzed); lookup perf-seal at
150M+ lookups **re-minted against the current hashbrown baseline**;
two-finger `//` merge differentially fuzzed. Entry gate: the fresh dhat
(L0) shows the attrs slice still pays.

**L7 — Trimmed thunk capture** (rides L3's free-var sets). `Suspended`
captures a micro-env `CapturedEnv { syms: Rc<[Symbol]>, vals: Box<[Value]> }`
of the expression's free variables instead of the whole Env. `with`-barrier:
any node whose resolution crosses a `with` scope falls back to whole-env
capture (m0-resolver fail-safe-to-Dynamic, verbatim). **Corrected sizing:**
what this frees is the env chunks pinned *solely* by never-forced thunks —
an **unmeasured fraction** of the old B slice; the two dhat attributions
(§II) are reconciled by the census, not by assertion. **Rec-scope spike
first, burden raised:** the `recursive → Promise` mechanism proves
mid-construction observation is designed-for; the spike must prove
creation-time probes sound for rec members or restrict phase 1 to
non-rec-member thunks. Probing at creation forces nothing (env probe
returns the stored thunk un-forced) → candidate ByteSufficient. Gated
behind `SUI_TRIM_CAPTURE=1` (the SUI_RESOLVE pattern). A/B axis:
**pinned-bytes census + footprint slope, never iters/s** — the axis
positional-frames never measured; land only wall-neutral-or-better AND
slope-improved.

**L8 — Tail-Apply release.** Scoped to the one form the shipped trampoline
does not cover (§II): on tail `Apply`, drop the caller's env Rc before
evaluating the callee (drop timing unobservable — no finalizers). A/B
against the **trampoline baseline** — the lever is the delta over what
already ships, or it re-records an existing mechanism (the redundant-
store-elision failure mode). Land on measured Improved only.

**L9 — Layered `//` chains** (cppnix #13987 analog; sequenced after L6 —
chunks are columnar shapes). Immutable sorted chunks chained (≤8 layers),
Rc-shared. **Corrected premises:** the 387M-slot/6.2 GB datum is dropped
(no committed source); cppnix's ~20% was measured against *its* eager-copy
baseline, while sui's shipped baseline is flatten-on-first-read +
release-parents — a chain that keeps ≤8 layers alive for O(depth) probes
**partially undoes that shipped retention fix**, so this lever can be
net-negative and must be A/B'd (churned + retained bytes) against current
main. The flatten cache must go layered too or it re-pays the copy.
Explicitly not a CPU lever (the ≤2.1% flatten fence holds); re-read
`get_sym` at build time (§II — the doc-comment is stale, the surface
churns).

**L10 — CA-spill of write-once payloads + seed-once strategy**
(EVAL-MEMORY §2 step 3, adopted). Forced sub-closure payloads and memo
entries that are content-addressed or provably pure spill to a disk-backed
store (redb/rkyv/mmap; super-cache-ci's PgStore is the fleet destination).
Converts the written-once dead pages from swap-thrash into keyed spill.
Purity-admission gate verbatim: impure-frontier subgraphs HOLD in RAM,
never release-and-recompute (the FIFO-evict counter-measurement stands).
Combined with the eval cache this is what makes "complete once within the
machine, hit forever after" U10's operating mode.

**L11 — The GC fork (held ceiling-crossing).** A conservative tracing GC
for the eval heap (the literal cppnix design), bundled with the explicit
eval/apply frame machine as its precise/conservative-roots precondition —
one bundle, never half-adopted, matured on a non-default branch until
77/77 + interleaved A/B green. **Trigger:** U10's residual footprint slope
after L6+L7+L10 remains above the completion line AND the residual is
dominated by still-pinned live-reachable structure (if it is dominated by
write-once payloads, L10 extends instead — smaller, in-thesis).
**Corrected pricing discipline:** the previously-quoted ~29% CPU price has
no committed provenance and is STRUCK; before this fork can be priced at
all, measure it — a GC_DONT_GC-style A/B on the marquee oracle, recorded in
the ledger, or the row stays `measured: Pending`. Qualitative direction
stands (cppnix is Boehm-GC'd; #13987's own data shows GC time is a large
cost fraction), and sui's census still deprioritizes it (Rc already
reclaims aggressively; zero cycles; the dominant pins are reachable, which
GC also cannot free). Restart-based sharding (L12) does **not** count as
evidence against this fork (§II — production restarts run *on top of* GC).
Wadler selector-thunk shortcutting rides this fork only.

### D. Orchestration axis

**L12 — Shard cascade + dirty-tree key policy.** N `sui --no-vm eval`
worker processes over disjoint attrs with footprint-threshold restart
(hydra/nix-eval-jobs/colmena-proven pattern — with the §II caveat named:
that prior art restarts GC'd workers). Engine-untouched; parity-neutral by
construction; retires the swap-death class for every multi-attr workload
permanently; the precondition that makes L13's daemon safe. Orchestration
lives outside sui-eval (`sui eval-jobs`; typed Rust, NO-SHELL, shigoto Dag
if ≥3 steps). Honest limit, stated: none of this touches U10 — one attr
cannot shard (DetSys's own named unsolved case). Cache-side work is the
**dirty-tree hashing policy only** (hash the dirty file set into the key —
motivated by hash-never-ignore discipline, not by any sui incident; §II);
the per-attr keys already ship. In-process Rc→Arc parallel forcing stays
fenced (3–4× ceiling on the wrong class, whole-graph memory-model
migration adjacent to two Discarded rows).

**L13 — Warm daemon + salsa-class incremental rebuilder** (after L12 —
restart bounding is the daemon's memory bridge). Salsa-class red-green with
durability firewalls: locked flake inputs are immutable-by-construction
HIGH durability; early cutoff on unchanged intermediates; byte-identical
reuse only (a hit returns previously-produced bytes — the shipped
eval-cache contract, so no force-order exposure). Serves U11 — the
daily-driver edit loop no Nix evaluator ships. Adapton's nominal naming
informs the key design. Constructive-trace sharing (celeiro/PgStore) is the
doctrinal extension after, landing in super-cache-ci, not sui-eval.

**L14 — Parallel lowering** (rides L3). Parse+lower is pure and per-file —
the parked tokio workers lower sources concurrently into the
content-hash-keyed Program cache. No Value graph touched; zero parity
surface.

**L15 — The eval cache (shipped, the payoff mechanism).** 3-tier,
407× MEASURED, verify-mode anti-stale, installable-only admission. Every
retention lever above exists to seed it once on U10.

### E. The fence (never re-attempt; red-row remediation may not reach here)

positional/flat env frames (net-negative, MEASURED) · thunk-env-repr
struct-shrink (Discarded; ceiling `PersistentLazyDesign`) ·
overlay-merge-structural (Discarded) · eval-time thunk-waste elision
(byte-safe fraction ≈0%; the tvix compile-time variant needs
ForceOrderProof and is not scheduled) · FIFO/eviction-and-recompute
(measured counterproductive) · hash-consing thunks · im_rc/RRB in new hot
paths · `//`-merge CPU micro-opt (≤2.1% fence) · whole-eval arena ·
in-process Rc→Arc parallel eval · generator-frame/CPS rewrite of the
tree-walker (tvix-measured ~neutral; stacker covers depth at zero profiled
cost; dedicated eval thread dodges the 8 MB main stack) · NaN-boxing the
tree-walker Value (≤2.3% cap) · `become` tail-call dispatch (parked until
the VM passes the byte oracle) · the order-preserving interner
(twin-reasoning-killed; stays killed — D1 in L6 is the honest replacement)
· `Eq`/`Hash` on force-capable Value (tvix abandoned-design).

---

## §IV The use-case matrix — the claim's falsifiable form

"Fastest across all use cases" is a claim with a per-class oracle, not a
headline (the 2026-07-17 harness proved why: **22.7× faster** on let-chains
and **9.4× slower** on deep recursion *in the same run*). Full spec lands
verbatim at [`benches/USE-CASE-MATRIX.md`]; this section is its canonical
condensed statement. The headline claim is defined as **every row at G2**;
a row's tier is what its evidence earns.

**Gate ladder:** **G0** completes inside the row's memory ceiling → **G1**
no-regress vs `benches/use-case-baseline.json` (deterministic work counters
where available; wall ± noise band where not) → **G2** beats nix on the
row's honest metric. Spawn-floor discipline: cppnix's ~70 ms process floor
is subtracted (`engine_ratio`), valid only when nix_eval ≥ 500 µs; U01 is
the exception (that class *is* startup — raw wall both sides).

| Row | Class | Oracle (nix) | sui today | Honest metric | Verdict (2026-07-21) |
|---|---|---|---|---|---|
| U01 | tiny expr (repl/CLI latency) | ~70 ms (spawn floor) | sub-ms | raw wall, spawn included | **WIN (G2)** — MEASURED |
| U02 | single-file hot-shape mix | spawn-subtracted | geomean **1.86×** | engine_ratio geomean | **WIN (G2, ex-U05)** — MEASURED |
| U03 | flake-eval small (kataFleet) | ~35 s | 35.2 s cold | cold wall | **TIE** — interleaved A/B to fix ratio |
| U04 | nixpkgs leaf drvPath (hello) | bytes ✓ | bytes ✓ | wall + byte-parity precondition | **PARITY** green; timed row *(to capture)* |
| U05 | deep-chain (fib-20 class) | 4.1 ms | 38.4 ms (**0.107×**) | engine_ratio | **LOSS 9.4×** — priced ceiling; G1-only until a lever outside the fence exists; the named open axis is L3 (the rowan re-walk), not thunk/env repr |
| U06 | attrset-heavy (mkDerivation) | 222 µs micro† | 42 µs† | macro-fixture engine_ratio + eval_expr work count | partial — macro mkDerivation-1k fixture *(to author)* |
| U07 | IFD-bearing eval | fixture *(to author)* | — | G0 first, then wall | **UNVERIFIED** — if G0-red it is a correctness workstream, forked out of this schedule immediately |
| U08 | string-heavy | *(to capture)* | +16.5% internal | engine_ratio | internally MEASURED; oracle row to capture |
| U09 | repeat warm (eval-cache hit) | *(capture warm nix)* | **0.03 s** vs 35.2 s cold | warm wall | **WIN pending oracle** — record it, don't assume it |
| U10 | whole-system cid (20,827 drvs) | **~107 s** / ~3 GB quiet (10.68 GB instrumented) | **never completes**; ~140 MB/s linear retained; freshest recorded footprint **25 GB** | G0 → footprint-slope series (L0 sampler) → wall; row ceiling 8 GB footprint + swap-death timeout | **CANNOT-COMPLETE (G0 RED)** — the headline red cell |
| U11 | dirty-tree edit-rebuild | *(to capture)* | = cold (35.2 s) | warm-after-edit wall | structurally cold — L13's forcing function |
| U12 | parallel-shard throughput | N workers / nix-eval-jobs | *(to capture)* | aggregate attrs/s at N ∈ {1,4,8} | UNMEASURED — L12's row |

† sub-comparability-threshold micro numbers, informational only.

**Reading:** green on interactive/shallow/warm, tied on small flake eval,
honest-red with a named ceiling on deep recursion, and **red at G0 on the
one class that is the actual product** (U10). U10 and U07 gate any
"fastest across all use cases" statement.

**CI wiring:** extend `sui-eval/tests/vs_nix_hotshapes.rs` (typed rows,
spawn-floor + comparability logic proven) + new
`sui-eval/tests/use_case_matrix.rs` (fleet matrix pattern: aggregate-failing,
`MATRIX.len() >= 12` coverage pin); results JSON records
`{row, profile, wall_us, engine_ratio, peak_footprint_mb, max_rss_mb,
work_counters, gate, verdict}`; U10/U12 nightly under the footprint sampler
— a timeout is a **recorded CANNOT-COMPLETE row**, not a flake; baselines
re-mint only with `--write-baseline` + a commit.

---

## §V The tier-honest lever ledger

Every lever in this strategy — research-sourced and our own — with its
expected magnitude, confidence, and **the measurement that gates it**.
Confidence never rounds up; STRUCK numbers are named as struck.

| # | Lever | Source | Expected magnitude | Confidence | Measurement (the gate) |
|---|---|---|---|---|---|
| L0 | Re-ground: fresh dhat + footprint-checkpoint sampler + matrix baseline | this pass (I9) | n/a — enables everything memory-side | — | dhat split on `582cede`+ recorded; `use-case-baseline.json` minted incl. a recorded U10 footprint run (replaces the STRUCK 64 GB/7 GB) |
| L1 | Fx/Symbol needed-bindings | our profile (sip-hash item, `eval.rs:418-444`) | 1–3% wall | HYPOTHESIS | interleaved A/B on U02/U03; Improved → land, Neutral → Discard |
| L2 | Path-Components intern/memo | our profile (~5% sample share) | share known, wall unknown (I6) | HYPOTHESIS | A/B wall U03 + U10-CPU checkpoints |
| L3 | Lower-once flat IR | our dhat/samples (40.7% bytes / 51% allocs / 21% wall class) + tvix's flat-program half; calibration: +12.6%/+9.5% per single site killed | double-digit wall on U02–U06; U10 slope (rnix share + churn) | PROJECTED | 77/77 both engines byte-diffed; alloc-call count delta; U02/U05 engine_ratio; no row regresses; U03 cold ≤ rowan |
| L4 | Symcache per-(Program, ExprId) | our ledger (+25.4% Deferred-era) | unknown vs hardened IDENT_CACHE baseline (§II) — re-base | MEASURED-STALE → re-base | soundness audit + interleaved A/B vs current main |
| L5 | Inline caches at Select | V8/Self ICs over shapes | single-digit to ~10% | HYPOTHESIS | measure-first; Improved only |
| L6 | Columnar + Shape attrsets | twin-reasoning survivor; cppnix/V8 corroboration | **unmeasured vs post-`fab4566` heap** — part of the old 45.2%-halving already banked | PROJECTED, re-base required | fresh-dhat attrs-slice delta; U10 slope delta; 150M-lookup perf-seal re-minted vs hashbrown; collation fuzz; `//` differential fuzz; **D1 resolved first** |
| L7 | Trimmed thunk capture | STG lesson + our census (51.8% never-forced) | unmeasured fraction of pinned env bytes (§II — attributions unreconciled) | HYPOTHESIS | **pinned-bytes census** (bytes held solely by never-forced thunks) before/after + U10 slope; rec-scope spike first (burden raised); wall-neutral-or-better |
| L8 | Tail-Apply release | tvix TCO analog, minus the shipped trampoline (§II) | small | HYPOTHESIS | A/B vs **trampoline baseline**: env-chain depth + slope + wall; Improved only |
| L9 | Layered `//` chains | cppnix #13987 (~20% vs *its* eager-copy baseline — not transferable as-is) | unknown vs sui's flatten-then-release baseline; **can be net-negative on retention** | HYPOTHESIS | churned + retained bytes A/B vs current main; CPU-neutral (2.1% fence); layered flatten cache included; re-read `get_sym` at build |
| L10 | CA-spill of write-once payloads | EVAL-MEMORY §2.3; nix-eval-jobs-adjacent | converts the written-once page fraction from swap-thrash to keyed spill | PROJECTED | purity-admission audit; U10 completes ≤ row ceiling with spill active |
| L11 | Conservative tracing GC fork | cppnix (Boehm) — the literal design | CPU price **UNMEASURED** (the ~29% figure STRUCK — no committed provenance) | UNMEASURED — must be priced before decided | precondition: GC_DONT_GC-style A/B on the marquee, recorded, else `measured: Pending`; trigger: post-L6/L7/L10 residual slope dominated by live-reachable pins |
| L12 | Shard cascade + dirty-tree key policy | hydra/nix-eval-jobs/colmena (restart **with** GC underneath — named) | retires multi-attr swap-death class; N×-class on U12 | MEASURED-class prior art | U12 attrs/s at N ∈ {1,4,8}; per-worker footprint under restart threshold |
| L13 | Warm daemon + salsa incremental | salsa/rust-analyzer/Adapton | U11 green; magnitude = edit-cone fraction | HYPOTHESIS (mechanism proven elsewhere) | warm-after-edit wall on U11; daemon footprint bounded by L12 policy; byte-identical reuse only |
| L14 | Parallel lowering | pure per-file property | hides lower time on cold multi-file | HYPOTHESIS | cold U03 wall A/B |
| L15 | Eval cache (shipped) | ours | **407×** repeat-eval | MEASURED | U09 row; the seed-once payoff for every retention lever above |

**Fenced levers (priced and closed — listed so the ledger is total):**
positional-frames (net-negative, MEASURED) · thunk-env-repr (Discarded,
`PersistentLazyDesign`) · overlay-merge-structural (Discarded) ·
thunk-waste-elision (~0% byte-safe, MEASURED) · eviction-and-recompute
(counterproductive, MEASURED) · hash-consing · im_rc/RRB new hot paths ·
`//` CPU micro-opt (≤2.1% cap) · whole-eval arena · Rc→Arc in-process
parallel · generator/CPS rewrite (tvix ~neutral) · NaN-boxed tree-walker
Value (≤2.3% cap) · `become` dispatch (parked on VM byte-oracle) ·
order-preserving interner (twin-reasoning-killed) · `Eq`/`Hash` on Value.

---

## §VI The execution schedule (dependency-ordered)

Serial landings, parallel tracks where subsystems don't collide
(L12 orchestration never touches `sui-eval`; `value.rs` and `eval.rs`
workstreams may run in parallel with serial landings). Every phase's exit
criterion is a measurement; every landing ships its `defperf-lever` row.

| Phase | Work | Depends on | Exit criterion (measurement) | Est. |
|---|---|---|---|---|
| **S0** | **L0 re-ground:** fresh dhat on `582cede`+; footprint-checkpoint sampler; matrix + `use_case_matrix.rs` + `use-case-baseline.json` (recorded U10 run replaces STRUCK numbers); capture missing oracles (U03 A/B, U04 timed, U08/U09/U11/U12); author U07 IFD fixture | — | baseline JSON committed; fresh dhat split recorded; U07 graded (G0-red → correctness fork, out of this schedule) | wk 1 |
| **S1** | ~~L1~~ + ~~L2~~ + the rec-scope spike — **ALL THREE RESOLVED 2026-07-21**: L1's target was dead code (§III correction); L2 path-memo measured NoImprovement on two harnesses, Discarded; the spike returned **NOT_WORTH_IT by census** — dead-let elimination is sound (let-only, plain-inherit excluded, `__findFile`/`__nixPath` blanket-kept) but addresses ~0.02% of thunk sites (417 thunk-allocating dead lets across ALL of nixpkgs vs 2.2M sites). The 51.8%-never-forced population is attrset VALUES — structurally required, enumerable — so the retention wall's lever is **L7 env-capture shrinking**, not elimination. S1 closed; its residue transfers to S3 (ExprId-gated free-var sets) and S5 (L7). | — | three ledger rows (Discarded x2 + census-kill); spike verdict recorded | done |
| **S2** | L12 shard cascade + dirty-tree hashing policy (`sui eval-jobs`) | S0 baselines for U12 | U12 attrs/s at N ∈ {1,4,8} recorded; multi-attr swap-death retired (worker restart proven) | wks 2–3, parallel track |
| **S3** | L3 flat IR: dual-engine → flip; then `(Program, ExprId)` thunk capture (separately gated); then L4 symcache re-base + L5 ICs measure-first; L14 parallel lowering | S0 | 77/77 both engines byte-diffed; no row regresses; alloc-call delta + U02/U05 engine_ratio recorded; rowan retired-behind-flag | wks 2–5 |
| **S4** | L6 columnar + Shape attrsets (D1 resolved first) | S0 (fresh dhat justifies) + S3 (stable ExprId for shapes-at-lower) | 3 parity gates green (collation fuzz, re-minted 150M perf-seal, `//` fuzz); fresh-dhat slice delta + U10 slope delta recorded | wks 4–6 |
| **S5** | L7 trimmed capture (`SUI_TRIM_CAPTURE` → default) + L8 tail-Apply release + L9 layered `//` (A/B vs flatten-then-release) | S3 (free-var sets), S4 (columnar chunks), S1 (spike) | pinned-bytes census delta + U10 slope; L8 Improved vs trampoline baseline; L9 churned/retained A/B non-negative | wks 6–9 |
| **S6** | L10 CA-spill + purity admission; **seed U10 once**; U09 serves it after | S4+S5 (slope down enough to bound) | **U10 completes ≤ 8 GB footprint (G0 flip)**; purity-admission audit green | wks 8–11 |
| **S7** | L13 warm daemon + salsa incremental | S2 (restart bounding) | U11 warm-after-edit wall recorded green; byte-identical reuse only | wks 9–14 |
| **S8** | L11 GC fork — **held**; fires only if U10 still G0-red after S4–S6 and the residual is live-reachable-pin-dominated | S6 verdict + **the GC price measured first** (GC_DONT_GC-style A/B, recorded) | non-default branch until 77/77 + interleaved A/B green; CeilingCrossing witnessed | decision, then 4–8 wks |

**Trajectory, tier-labeled:** after S0–S2 — U01/U02 green (MEASURED),
U09/U12 green (MEASURED-class), U03 fixed by A/B, multi-attr swap-death
retired; after S3–S4 — U06 green (PROJECTED), U05 gap narrows by whatever
share of the 21%-wall class it carries (HYPOTHESIS until A/B'd), U10 slope
measurably down (gated by the fresh dhat, not the stale one); after S5–S6 —
best case U10 completes and seeds the cache → the marquee becomes a 0.03 s
daily hit (MEASURED mechanism); after S7 — U11 green (HYPOTHESIS on
magnitude, salsa-proven mechanism). S8 fires only on evidence. Every step
lands a ledger row or an honest Discard; either outcome is real ground.

---

## §VII The single next move

**S0's first commit: re-ground the baseline.** Run the cid marquee on
current main (`582cede`+) under dhat **and** the footprint-at-checkpoint
sampler —

```
sui --no-vm eval --raw --no-eval-cache \
  .#darwinConfigurations.cid.system.build.toplevel.drvPath
```

— and land, in one commit: the fresh dhat split (the first profile of the
heap that actually exists post-`fab4566`/`582cede`), the recorded U10
footprint-slope series, and the minted `benches/use-case-baseline.json` —
replacing every STRUCK number with a committed one.

No memory-side lever (L6/L7/L9/L10) may be sized, A/B'd, or landed before
this number exists. It is the dependency root of the entire schedule, it is
one run, and it converts this strategy's largest remaining unknown — *what
the heap looks like today* — from an inherited stale document into
measured ground.

---

## §VIII. The control pillar — steerability (afinar) over the whole campaign

Every lever this strategy lands terminates in a **steerable surface**, not a
compiled-in constant. The fleet mechanism is **afinar** (the live-tunable
doctrine; see the `afinar` skill and `theory/AFINAR.md`): knobs declared as
typed panels, driven over `afinar_knob_*` MCP tools, shadow-gated
(`dry_run` + `write_enabled`), reject-at-border.

Applied here (DESIGN — none of this is built; the rule-layer vocabulary
`(defeager-class …)` / `(defshape …)` / `(defretention-policy …)` does not
exist in-tree yet, verified by grep 2026-07-21):

- **Optimization rules are declarative lisp layers**, carried on
  `shikumi::TieredConfig` (already a sui dependency — `sui-daemon/Cargo.toml`);
  the runtime tier IS the afinar patch surface. Rules compile at startup to
  typed Rust tables — lisp never interprets in the hot path (the BUILD.md law).
- **Steering is per use-case class**: the matrix rows (U01–U12) are the
  addressable scopes — "repl class: eager-cheap ON; closure class: retention
  aggressive." A knob flip is measured by the row it targets.
- **THE PARITY-TYPED KNOB SPACE (the law this campaign adds to afinar):** a
  knob is live-patchable **only if its technique class is `ByteSufficient`**
  in the perf ledger's taxonomy; a force-order-shaped knob is refused at the
  patch border until the parity corpus has gated that specific rule. Steering
  may change *how fast*; it structurally may not change *the bytes*.
- **Every tuning exchange lands in the ledger** as a `(defperf-lever …)`
  claim with its measurement — steering (afinar), mechanism (rule layers),
  and measurement (the matrix) are three lisp surfaces of one loop.

This is the axis no cppnix can follow: their evaluation strategy is
compiled-in C++; ours becomes a conversation — flip, shadow, measure,
promote, attest — with honesty enforced by the same typed gates that run the
rest of this document.

[`benches/USE-CASE-MATRIX.md`]: ../benches/USE-CASE-MATRIX.md
[`sui-spec/specs/perf.lisp`]: ../sui-spec/specs/perf.lisp
[`EVAL-MEMORY.md`]: ./EVAL-MEMORY.md
[`PERF-CLAIM-LEDGER.md`]: ./PERF-CLAIM-LEDGER.md
[`PERF-ARSENAL.md`]: ./PERF-ARSENAL.md
[`SUI-EQUIVALENCE.md`]: ./SUI-EQUIVALENCE.md
[`DARWIN-PARITY-CAMPAIGN.md`]: ./DARWIN-PARITY-CAMPAIGN.md
