# ENV-RESOLVE — closing the tree-walker deep-recursion regression

**Status: DESIGN (M0 not yet landable in one pass — see §7).** Grounded +
adversarially-verified via a big-bang design pass (2026-07-18). This is the
tier-honest plan for the next real perf build; nothing here is shipped yet.

## 1. The measured problem

The `vs_nix_hotshapes.rs` harness (release, `sui_eval::eval`) measures the
**tree-walker** at `engine_ratio 0.107×` on `rec_fib_20` = **~9× SLOWER than
cppnix on deep recursion**, while WINNING 1.7–22× on allocation-bound shapes.
Storm A (referenced_idents) + Storm B (overlay-flatten) are both ≤0.1% of eval —
neither is the lever. (Constant-factor micro-wins already landed: the lazy typed
trace-frame + the force_inner clone→move, ~a few % on fib; they do not close the
gap.)

**Measurement-validity (checked, not assumed):** `sui_eval::eval` (eval.rs:447
`eval_with_file`) parses with rnix and evaluates the AST on a tree-walker
`Env::new()` — pure tree-walker, no VM compile path (the `sui-bytecode` dep is
only the flake/builtin bridges). So the 0.107× IS a tree-walker number; the
env-refactor correctly targets the real regression.

## 2. The premise correction (what sui's env actually is)

sui's tree-walker env is NOT a naive HAMT-clone-per-call. `Env = Rc<EnvInner>`;
`EnvInner.bindings = im_rc::HashMap<Symbol, Value>` (a persistent HAMT keyed by
**interned `Symbol(u32)`**, value.rs:2502). Two properties are already
better-than-naive and must be KEPT:

- `child()` (value.rs:2544) = `bindings.clone()` = **O(1) structural sharing**
  (HAMT root refcount bump, not a deep copy).
- lookup (`lookup_fast`, value.rs:2714) = one HAMT probe by `Symbol(u32)` — **no
  string hashing** (sui already interns).

**The real cost vs cppnix:** (a) per-`bind` the HAMT insert path-copies ~log32
branch nodes AND re-interns the name; dhat found these im_rc branch-node allocs
dominate sui's eval heap (~2× peak vs nix). (b) lookup is an O(log32 n) trie
probe vs cppnix's O(1) positional `values[displ]` array index. cppnix resolves
every var to a `(level, displ)` pair at PARSE time (`ExprVar::bindVars` over a
`StaticEnv` chain) and stores frames as fixed-size `Value*` arrays with a parent
pointer — O(1) extend, O(level) chase, zero hashing.

## 3. Chosen approach — Angle 3: an engine-neutral `sui-resolve` side-table

A single parse-time resolver pass (`bind_vars` over rnix + a cppnix-shaped
`StaticEnv` chain, `is_with` frames as barriers) emits a typed
`enum Resolution { Lexical{..} | Dynamic{name} | Global }` into a
`SyntaxNodePtr`-keyed side-table, consumed at eval.rs's Ident arm. Keyed by the
one genuinely-shared cross-engine asset, `sui_intern::Symbol`.

**Rejected (evidence-based):** Angle 2 (full parse-time `(level,displ)` + array
frames) rewrites frame shape around every fixpoint site (the `concatLists: got
null` surfaces) and has a silent-wrong-answer failure mode on a mis-resolved
slot. Angle 1 (parent-pointer two-pass chain) re-implements lexical-before-with
precedence in new control flow (a re-derivation needing a force-order proof) and
trades away sui's deliberate O(1) `child()`. Angle 3 is the only one where the
`with` subsystem is invariant-**by-omission**.

## 4. Why it is parity-safe on `with` (the hardest hazard)

`bind_vars` marks any ident not lexically resolvable in an enclosing
let/rec/lambda as `Dynamic{name}`; every `Dynamic` ident routes **VERBATIM**
through today's unchanged `lookup_fast` with-chain (value.rs:2714 — reversed-with,
full-chain `force_value` per 2e44038, `lookup_fresh` stale-cache) + the M2.6
ROOT #4a lazy-namespace deferral (eval.rs:1244). Soundness rests on the rule a
lexically-resolvable name is NEVER a with-var — which IS today's runtime order
(bindings probed first, any lexical hit beats every with). **The one sharp edge:**
`bind_vars` MUST be conservative — on ANY uncertainty emit `Dynamic` (fail-safe =
slower-but-correct, never wrong). Gated by `control_with_lexical_precedence` +
`with_shadowing_*` (eval.rs:6067–6099).

## 5. Reuse map (Care #4 — reuse-first)

KEPT UNCHANGED: the entire tree-walker env (`EnvInner`), `child()` O(1) share,
`lookup_fast` with-chain, `Expr::With` lazy site, the two-phase let/rec fixpoint,
`WithScope` + its shared cache, `eval_file`/EVAL_FILE_STACK, pos.rs AttrPositions.
REUSED as a design template only (verified NOT liftable — welded to `emit()`,
String-keyed): the VM's `resolve_local`/`StaticEnv` algorithm (compiler.rs:614+).
NET-NEW: one `sui-resolve` crate (~400 LOC) + a ~1-site consume at eval.rs's Ident
arm.

## 6. Phased plan

- **M0 — Symbol-precompute side-table, tree-walker only, no env change** (behind a
  `--resolve` flag, default off): a `Lexical` ident carries its precomputed Symbol
  (skips re-intern) and only `Dynamic` idents touch the with-chain; a table miss
  falls back to today's runtime probe (slow-but-correct). **Parity-by-construction**
  — a pure hash-key-caching opt over the identical im_rc map + identical
  `lookup_fast` fallback (a byte-diff is a SUFFICIENT proof). `rec_fib_20` is
  verified `with`-free pure-lexical (vs_nix_hotshapes.rs:81) so it's exactly on
  this fast path.
- **M1 — additive positional frame overlay** (the real per-lookup hash kill): a
  `frames: Vec<Rc<[Value]>>` overlay; `Lexical{up,slot}` reads
  `frames[len-1-up][slot]` (O(up)+O(1)) instead of the HAMT probe. Slots are
  Rc-clones of the SAME thunks the im_rc map holds (single source of truth; the map
  stays authoritative + the with/fixpoint home). **Changes-resolution-mechanism —
  a byte-diff is NECESSARY BUT NOT SUFFICIENT**; needs the coupling proof (frame
  slot ≡ the map's Rc-shared thunk, so recursive-let `update_env` mutates one thunk)
  + the `eval_file`/AttrPositions mirror check (darwin options.json bytes).
- **M2 — widen + measure on real nixpkgs**: run a live nixpkgs eval under the
  overlay; add a periodic-flatten fallback ONLY if a measured deep-chain
  regression appears (O(up) chase vs HAMT probe on very-far binders); then default
  the flag on.
- **M3 (aspirational, separate follow-up)** — decouple the VM resolver from
  `emit()` so both engines consume the one Symbol-keyed `sui-resolve` table (one
  shared scope model). Explicitly NOT claimed today.

## 6a. M0 result (built 2026-07-18) — parity-proven, MEASURED NULL

M0 is **SHIPPED behind `SUI_RESOLVE=1` (default off)**: the `sui-resolve` crate
(`Resolution{Lexical{sym}|Dynamic}` + a `bind_vars` pass over a `StaticEnv` chain
with `is_with` barriers, fail-safe-to-Dynamic; 16 unit tests) + a
`(source_id<<32)|offset`-keyed side-table + a purely-additive `Env::lookup_lexical_sym`
+ the eval.rs Ident-arm fast path. **Parity-by-construction PROVEN both ways:**
flag-off = baseline exactly; flag-on `SUI_RESOLVE=1` = byte-identical (all
`with_shadowing_*` / `control_with_lexical_precedence` / `eval_recursive_let` /
`error_infinite_recursion` tests green + drv_path_parity + hello/coreutils drvPath).

**But the measured perf win on `rec_fib_20` is NULL (−0.5%, within jitter)** — even
though the fast path fires on every lexical lookup (fib20 records 7 `Lexical` entries,
all hit). **Finding: re-intern elision alone is NOT the fib bottleneck** (as §8
warned) — fib's cost is recursive-call machinery + thunk force + arithmetic, not
ident re-interning. The real per-lookup win is **M1** (positional frames killing the
HAMT probe itself, not just the re-intern). M0's value is the parity-proven resolver
**foundation M1 consumes** + this measurement redirecting the lever. Do NOT default
the flag on for a null win — it earns default-on only when M1 makes it move fib.

## 6b. M1 result (built + measured 2026-07-18) — coupling PROVEN, perf NET-NEGATIVE, NOT shipped

M1 (the positional-frame overlay) was fully built + verified, then **NOT committed**
per the §8 honest-stop escape. Two hard findings:

1. **The coupling is AIRTIGHT (the design's #1 risk — RETIRED).** `Resolution` extended
   to `Lexical{sym, up, slot}`; an additive `Env.frames: Vec<Rc<[RefCell<Option<Value>>]>>`
   whose slots are `bindings.get(sym).clone()` — the SAME `Rc<ThunkInner>` the im_rc map
   holds; the two-step install(empty)-before-capture / fill(in-place)-after ensures the
   fixpoint thunks capture the shared frame `Rc` and `update_env` back-patches the shared
   thunk interior (never replaces the map entry). 12 recursive-fixpoint tests (fib, rec-attr
   `s.f` self-ref, mutual rec a/b + isEven/isOdd, chained let, deep 12-up chains, pattern
   defaults incl `@`-bind forward-ref, with-barrier up-counting) all byte-identical under
   `SUI_RESOLVE=1` and flag-off; fib20 = 65672 frame hits / 0 misses. Parity byte-identical
   both modes (all suites + drv_path_parity 6/6). So the "dual-store coupling" hazard is
   provably solvable — but that is not the same as a win.

2. **Net-NEGATIVE perf (measured, median-of-31).** The array read IS faster per-lookup, but
   per-binder-scope **frame construction** (`Rc<[RefCell]>` heap alloc + N map-probes to fill
   + a frame-vec Rc-bump every `child()`) costs MORE than the interned-Symbol HAMT probe it
   removes on call-heavy code: `rec_fib_20` **+7%**, `list_map_1000`/`foldl_1000` **+32–39%**;
   `deep_let_100` (1 frame) neutral/faster. The regression scales with FRAME ALLOCS, not
   lookups — exactly §8's "alloc-side" risk.

**Verdict + the REAL redirect:** the HAMT probe was already cheap (interned `Symbol(u32)`);
the lookup was never the bottleneck. **The lever is NOT faster lookup — it is removing the
per-call frame ALLOCATION.** A real M2 must make a positional frame near-free: arena/slab/
pooled frames, a fixed-capacity inline (SmallVec/stack) frame, or reusing the closure env's
storage — only then does a positional read beat the HAMT probe. M1's frame-overlay code is
the net-negative part a real M2 REPLACES (not extends); it was deliberately not committed.
The `(up,slot)` resolver extension + this proven coupling recipe are the reusable parts.

## 6c. M2 verdict (designed 2026-07-18) — NO clean proven win; the fib gap is largely inherent

A big-bang design pass (recon: M1-alloc-cost · cppnix-arena · sui-capture-constraint →
adversarial judge) assessed the whole cheap-frame space. **Verdict: there is no clean,
proven, net-positive, no-regression M2 slice to build now.** The space collapses:

- **cppnix's cheapness is a tracing-GC property sui can't copy.** cppnix bump-allocates
  16-byte frames + collects them wholesale; a captured frame stays alive transparently.
  sui uses `Rc` (no GC), so a bump-arena that frees on return dangles captured frames.
- **Frame-representation swaps all fail the bar.** (A) inline/SmallVec — on `fib`, escape
  is **~100%** (the arg BinOp thunks capture the param frame; `thunks_created==thunks_forced`,
  0% waste), so inline slots heap-promote on capture and buy nothing exactly where the gap
  is. (C) whole-eval bumpalo arena — a **memory regression** on nixpkgs (no GC to reclaim;
  defeats the Rc reclamation the live-census proved), i.e. trading a time regression for a
  memory one, which the never-regress rule forbids. Env-flattening — already
  `/twin-reasoning`-ruled-out (EVAL-MEMORY.md:87: O(1) scope-push → O(n), "wrecks eval time").
  (B) slab/pool — the ONLY sound swap (Rc-driven, capture-safe), but **UNPROVEN**: a bump-
  alloc still touches the allocator + the Rc refcount, and M1 already proved a representation
  change can *regress* the already-cheap interned-Symbol HAMT probe. Per never-ship-a-regression,
  it is a **measure-first follow-up**, not a buildable win.
- **Allocation-side levers are exhausted or small.** The judge named "skip the redundant 2nd
  thunk-store" the #1 lever — but that is **ALREADY LANDED** (57da0d79 "C-store redundant-Store#2
  skip"; HEAD value.rs:1685 `!was_thunk_before_loop` early-return + `store_evaluated_owned`; the
  `thunk_store_redundant` counter measures how often it's *already elided*, not a pending
  redundancy — a doc-tag-vs-source correction). child()+bind fusion is a per-call `make_mut`
  (~small, "not the 9× gap"). demand-thunking is parity-edged (must stay conservative-to-thunk
  on anything observing a fixpoint) + already partially landed (−6.5% variant).

**The honest bottom line:** the ~9× deep-recursion gap is **substantially the PRICE of sui's
persistent-lazy design** — a fresh `Rc<EnvInner>` per call + an `Rc<ThunkInner>` per non-constant
arg, the cost of the O(1)-persistent-`child()` + call-by-need env that cppnix's tracing GC gets
"for free" and sui's Rc cannot. sui *wins* 1.7–22× on allocation-bound shapes and pays this on
deep recursion — a deliberate, correct trade, not a bug. The one remaining credible path
(slab/pool frames) is an **unproven** measure-first experiment, explicitly NOT a rush. **M2 is
not built; the fib gap is accepted as the honest current state until a measured slab/pool
experiment proves a net-positive.**

## 7. Tier ledger (never round up)

- **SHIPPED:** the im_rc env + O(1) `child()`; `lookup_fast`; the M2.6 ROOT #4a
  with-deferral; the #2 harness with `rec_fib_20` at 0.107×; the VM resolver as a
  template (not liftable). **NEW: M0 `sui-resolve` + the Ident-arm consume (flag-gated
  `SUI_RESOLVE=1`, parity-by-construction proven both modes) — the M1 foundation.**
- **MEASURED NULL (M0):** re-intern elision does not move `rec_fib_20` (−0.5%).
- **MEASURED NEGATIVE (M1, §6b):** the positional-frame overlay is +7% fib20 / +32–39%
  call-heavy — the per-call frame ALLOCATION costs more than the HAMT probe it removes.
  Coupling proven airtight (the #1 risk retired); code NOT committed (§8 honest-stop).
- **M2 ASSESSED — NO clean proven win (§6c):** the cheap-frame space collapses. Representation
  swaps regress (M1, inline-escape) / memory-regress (arena, no GC) / are unproven (slab/pool);
  the allocation levers are already-landed (redundant store, 57da0d79) or small (fusion) or
  parity-edged (demand-thunking). **The ~9× fib gap is substantially the PRICE of sui's
  persistent-lazy design** (`Rc<EnvInner>`/call + `Rc<ThunkInner>`/arg — the O(1)-persistent
  `child()` cppnix's GC gets free and Rc cannot). sui wins 1.7–22× on alloc-bound shapes, pays
  this on deep recursion: a correct trade. **NOT built** (never-ship-a-regression: no proven
  net-positive slice exists).
- **UNPROVEN FOLLOW-UP:** slab/pool frames (the only sound representation swap) — measure-first,
  not a build; M1 proved a representation change can regress.
- **DESIGN (unwritten):** M3 (VM shares the resolver table — aspirational).
- **Not-landable-in-one-pass:** even M0 is a new crate + a resolver pass — a real
  prerequisite, not a one-careful-pass edit. Rounding it to "landable now" is the
  path-of-least-resistance sin.

## 8. Open risks

- Conservative-`Dynamic` classification is the sharpest edge: a `bind_vars` bug
  mis-marking a with-shadowable name as `Lexical` silently changes scoping.
  Fail-safe-to-Dynamic is mandatory; `with_shadowing_*` is the guard.
- `SyntaxNodePtr` key stability across resolve→eval (thunk-body/import re-parse);
  the table-miss→runtime-probe fallback must be genuinely correct (miss = slow,
  never wrong) — needs a test.
- M1's dual-store (frame array + im_rc map) is a second source of truth; if the
  Rc-shared-thunk coupling isn't airtight, the recursive-let fixpoint can diverge —
  a real invariant to PROVE, not a byte-diff to check. If hard to guarantee, ship
  M0 only and treat the array as optional.
- Deep-chain lookup regression (all approaches share it): O(up) walk vs O(log32 n)
  probe on very-far nixpkgs binders — M2 must MEASURE, not assume; the win is
  alloc-side and could be flatter than 9× on lookup-heavy deep-scope code.

## 9. Verify loop (every pass)

Per the sui Parity Method (theory/BUILD.md §II.1): land behind the `--resolve`
flag, flip on ONLY after byte-verify. Each pass: `cargo test -p sui-eval` (the 12
with-scope tests + recursive-let + infinite-recursion) + `tests/lang_corpus.rs`
golden JSON + `SUI_TEST_ONLINE=1 drv_path_parity.rs` + the sealed parity_corpus +
`sui eval --no-vm` hello `.drvPath` byte-identical (the `j8q5j0x4` CLOSED row) +
the broad basket 102/104 + the corpus 26-match floor NEVER moves. Measurable gate:
the #2 harness `rec_fib_20` sui_us DOWN / engine_ratio UP from 0.107×.
