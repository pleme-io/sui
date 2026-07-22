# Dark-side optimization — the confirming profile (internal research, 2026-07-22)

The STRATOSPHERE §8 dark-side hypothesis ("the cost is allocation churn + env
representation; the lever is arena / parent-pointer env-frames") was a claim.
This is the **data** — a symbolicated CPU profile of the tree-walker (the
byte-parity oracle) on a heavy env-binding-churn workload.

## Method (load-robust, reproducible)

- Binary: `target/release-profiling/sui` (`[profile.release-profiling]` — `strip=false`, debuginfo kept; `release` is stripped).
- Workload (max env churn): `builtins.foldl' (acc: i: let a = i + 1; b = a * 2; c = b + acc; d = c - a; in d) 0 (builtins.genList (i: i) 1500000)` — 1.5M iterations, each a 4-binding `let` body ⇒ `Env::child` + 4×`Env::bind` + 4 thunks per iter. This isolates the env/thunk/alloc hot path.
- Engine: `sui eval --no-vm -E` (the tree-walker — the default + byte-parity oracle; the engine that must stay byte-exact).
- Profiler: `/usr/bin/sample <pid> 18 1` (macOS, no sudo; 1 ms interval, 18 s window). Calibration run: **5.07 s user CPU**, 26 MB peak RSS, 54.5 B instructions retired.
- Leaf self-time read from the sample's "Sort by top of stack" section; idle tokio-worker kernel waits (`__psynch_cvwait`/`__ulock_wait`/`kevent`) excluded. n = 12,493 work self-samples.

## Result — self-time buckets (main-thread work only)

| % | samples | bucket | dark-side lever |
|---:|---:|---|---|
| **23.8%** | 2972 | ALLOCATOR (mimalloc malloc/free + memmove) | shared — driven by both levers below |
| **18.6%** | 2327 | ROWAN / rnix CST re-walk | **Lever B — flat IR (`eval_ir`)** |
| 12.2% | 1529 | interpreter dispatch (`eval_expr*`, `apply*`, `referenced_idents`…) | — |
| **11.0%** | 1377 | `im_rc` HAMT node machinery (`bitmaps::Iter::next` = biggest single non-alloc leaf) | **Lever A — env frames** |
| **8.6%** | 1076 | hashing + hashmap (HAMT hashes every ident per bind/lookup) | **Lever A** |
| **5.8%** | 728 | `Rc::make_mut` (COW the HAMT per `bind`) + `drop_slow` | **Lever A** |
| 5.5% | 690 | other interp/misc | — |
| 4.4% | 544 | TLS (`_tlv_get_addr`) + trace guard overhead | cheap separate lever |
| 3.2% | 406 | thunk machinery | Lever A-adjacent |
| **2.7%** | 333 | `Env::bind`/`child`/`lookup` | **Lever A** |
| 2.7% | 335 | Value ctor/dtor/clone | — |
| 1.4% | 176 | interner | — |

## The two confirmed dark-side levers

**Lever A — env representation: `im_rc` HAMT → parent-pointer flat frames (cppnix's `Env* up` model).**
The env cost is *spread across four buckets* that a flat-frame arena collapses to near-zero:
HAMT machinery 11.0% + Rc-COW/drop 5.8% + hashing 8.6% + env-ops 2.7% = **~28% directly**, **plus** a large share of the 23.8% allocator bucket (every `Rc::make_mut` on `bind` allocates fresh HAMT nodes). Realistic Lever-A share: **~30–38% of tree-walker self-time.**
Root, confirmed in source: `sui-eval/src/value.rs:2578` — `EnvInner.bindings: FxHashMap<Symbol, Value>` where `FxHashMap = im_rc::HashMap<…>` (persistent HAMT). `child()` (2627) clones the HAMT O(1); `bind()` (2671) does `Rc::make_mut(&mut self.0).bindings.insert(…)` — COW-per-bind is the churn.
A parent-pointer frame `{ slots: [Value; N], up: Option<Rc<Frame>> }` with lexical **level/index** addressing removes: the hashing (index, not hash), the HAMT (array, not tree), the COW (a frame is immutable after build; children just point `up`), and the bitmap iteration. This IS the "arena work."

**Lever B — AST model: rowan CST re-walk → flat IR (`eval_ir`).**
ROWAN/rnix re-walk = **18.6%** directly, plus its share of the allocator bucket (`rowan::cursor::NodeData::new`, `cursor::free` allocate/free cursor nodes each eval). Already prototyped: `sui-ir::eval_ir` measured **2.58× geomean** byte-identical on 4 pure workloads (`sui-ir/tests/perf_ir_vs_tree.rs`). Tracked as STRATOSPHERE M5.

Together A+B attack **~50–60%** of tree-walker self-time — consistent with the ~7× user-CPU gap vs cppnix, which uses *both* a flat env (displacement/level indexing) *and* a flat/compiled representation.

## Why these are DARK SIDE (the correctness boundary, STRATOSPHERE §8)

Both levers are **byte-RISKY**: they change *how* values are produced, and Nix's subtle scoping (dynamic `with`-scope precedence, `rec` self-reference/fixpoint, `let` shadowing, thunk blackholing) is exactly where a flat-frame model can *silently* change an observable value if scoping is mismodeled — the same class of divergence the two sui engines already hit (VM defers string-context; tree-walker tracks it). Therefore:

- Each lever ships as an **opt-in, non-default engine**, tier-labeled.
- Promotion to default is gated on **byte-parity** — a whole-corpus differential vs the tree-walker oracle (`sui parity` + the lang corpus + build-parity), never on a spot check.
- Never a silent wrong answer: on any divergence the experiment stays non-default and the divergence is a red gate to root-cause.

## Prior-art reconciliation — what's already ruled out (do NOT re-tread)

`docs/ENV-RESOLVE-DESIGN.md` (2026-07-18, measured) already attacked this exact
env cost and settled the space. The profile above AGREES with it; combined, they
sharpen the honest lever set:

- **`sui-resolve` M0 (parse-time `Resolution{Lexical{sym}|Dynamic}` side-table)** —
  SHIPPED behind `SUI_RESOLVE=1`, byte-parity-proven both flag states. But
  re-intern elision alone = **NULL** on fib. It's the *foundation*, not a win.
- **Positional-frame overlay M1** — coupling proven airtight (the #1 risk RETIRED),
  but **NET-NEGATIVE** (+7% fib, +32–39% call-heavy `list_map`/`foldl`) because
  per-call frame *allocation* (`Rc<[RefCell]>` alloc + fill) costs more than the
  interned-`Symbol(u32)` HAMT probe it removes. **NOT committed.** ⇒ the lever is
  **allocation, not lookup**.
- **M2 space collapsed:** cppnix frames are cheap only via *tracing GC* (bump-alloc +
  wholesale collect + transparent capture); sui's Rc can't copy it. A whole-eval
  bumpalo arena **memory-regresses** nixpkgs (no GC to reclaim) — trading a time
  regression for a memory one, which never-regress forbids. Inline/SmallVec frames
  heap-promote on the ~100% capture escape on fib. Redundant-store-skip already
  landed (57da0d79). **The ~9× deep-recursion gap is substantially the PRICE of
  sui's persistent-lazy, GC-free design** — a correct trade (sui wins 1.7–22× on
  alloc-bound shapes).

### The one open, sound, measure-first survivor

**A slab/pool allocator for the hot Rc-allocated types** (`Rc<ThunkInner>` per
non-constant arg is the most-allocated; then `Rc<EnvInner>`, then `im_rc` HAMT
branch nodes): allocate `RcInner<T>` from a freelist instead of `malloc`, but keep
Rc's refcount driving reclamation (freed back to the pool on drop, **not** leaked
like a bump-arena). This is the one path `ENV-RESOLVE-DESIGN.md §6c` left open:
*"slab/pool frames (the only sound representation swap) — measure-first."*

Crucially, this is a **`ReprSwap`-class** change (storage representation, no
observable change) ⇒ `ProofTier::ByteSufficient` ⇒ **byte-safe by construction**,
NOT byte-risky. Its only risk is *perf* (could regress like the bump-arena's memory
did), so it is measure-first per never-ship-a-regression — never default until a
proven net-positive with NO memory regression on both alloc-bound AND
deep-recursion shapes. It attacks the **23.8% allocator + 5.8% Rc-`drop_slow`**
buckets directly, at their root, without touching semantics.

### The clean split the dark-side vocabulary must encode

The operator's "dark side / arena work" intuition splits into two typed axes:

1. **byte-SAFE, perf-risky (`ReprSwap`/`ByteSufficient`)** — the pooled/slab
   allocator (Track 1). Correctness is free; the gate is a measured no-regression
   win. This is the untried survivor.
2. **byte-RISKY (the true dark side — `ForceOrderChange`/`ResolutionChange`, or a
   whole alternate engine)** — `eval_ir` promoted from "wired into nothing" to an
   opt-in engine (Track 2): attacks the **18.6% rowan re-walk**, measured **2.58×**
   byte-identical on pure workloads, needs a **whole-corpus byte-parity differential**
   before it can ever be default. This is where "full throttle" has the most headroom.

Both tracks are honestly captured by making **byte-risk tier a first-class typed
field** on each optimization row (the `/vocabularify` deliverable), so an unproven
lever cannot claim "promoted" and a byte-risky lever cannot claim `ByteSufficient`.

## Raw artifact

`scratchpad/sui-envchurn.sample` (643 KB, full call graph + leaf leaderboard).
