# sui-eval memory: the honest plan (streaming, not a substrate)

> Big-bang recon + adversarial verify (2026-07-12) against `sui` `main` @ `b3568bb`.
> **The headline correction: eviction/GC does NOT fix the cid-eval OOM, and a
> config-tiered memory substrate is NOT justified.** Three overclaims were caught by
> the adversarial pass and are recorded in §5 so they never silently return. This is a
> `sui/docs/` eval-engine plan, deliberately NOT a minted theory doctrine (minting one
> for a ~2-change fix would be the over-abstraction the verdicts warn against).

## 0. Ground before building (Care #1) — the failure mode is ENVIRONMENT-DEPENDENT

**UPDATE 2026-07-15 (`f87fd07`, release build, `/usr/bin/time -l`): M0-A re-run FLIPPED —
today the cid toplevel eval IS a genuine RSS OOM.** Measured: peak RSS **14.57 GB**,
killed by signal (macOS Jetsam) at **18.4 min** — NOT the 30-min `timeout`, and sui
printed no error of its own. Machine: 32 GB RAM, and swap was already **12.4 GB
committed by other work** before the run. So sui's 14.6 GB peak on a pre-saturated
32 GB box → Jetsam SIGKILL. The 2026-07-12 run (below) reached 40 min WITHOUT OOM on a
less-loaded machine — so **the cid eval is blocked by BOTH ~14.6 GB peak memory AND
40+-min eval time, and WHICH one kills it depends on machine load.** Neither has a cheap
byte-safe win: the double-store collapse (§1) is re-verified as a ~1–2 GB reprieve only
(the heavy `NixAttrs`/`Vec`/`NixString` payloads are `Rc`-SHARED between `cache:
Box<Concrete>` and `repr: Evaluated(Box<Value>)` — `clone()` bumps the Rc, so only ~2
enum-shell boxes/forced-thunk duplicate, NOT the payload). The 14.6 GB dominant term is
the INHERENT live working-set (§1) confirmed against the types. Real fixes remain
streaming-release (memory) + hot-path localization (time), both multi-step; a warm
cross-run eval-output memo (§4 step 3 / atatame-adjacent) would make the expensive eval a
one-time cost — the structural sidestep.

**Prior finding (2026-07-12, `SUI_PERF_TRACE` cid eval, 40-min window @ `b3568bb`):
the blocker was un-traced EVAL-TIME slowness, NOT memory, on a less-loaded machine.**
Retained — it is the OTHER half of the environment-dependent picture, not superseded.

Dispositive trace evidence:
- **NAR-hash storm: FIXED** — `redundant:0`, `nar_time` plateaus at ~30s (the content-
  address memo holds; not the bottleneck).
- **IFD-realize: fast** — 16 realizes, 0.01–0.09s each, **cumulative 0.4s**. The daemon
  substitutes quickly. Not the bottleneck.
- **~39 of 40 minutes are in NEITHER traced path** — the eval reached only ~600 NAR
  calls + 16 realizes in 40 min and produced no drvPath. The dominant cost is raw
  eval-time in the **un-instrumented hot path**: thunk forcing / `NixAttrs::Overlay`
  re-materialization / `Env` churn across the 50+-deep module-system + nixpkgs overlay
  fixpoint (the amplifiers named in §1, but as a TIME cost here, not a memory one).
- **NOT OOM-killed** in a 40-min window — the earlier "swap exhaustion" report was an
  earlier run / external memory pressure, not the current symptom.

**Redirect:** the next diagnostic is the **eval hot path**, not memory — instrument the
`perf.rs` counters (`ThunkForce`, `EnvClone`, overlay-flatten) to localize where the 39
un-traced minutes go, then fix the dominant eval-time amplifier (prime suspect:
`NixAttrs::Overlay` re-flattening / retaining both parents, `value.rs:1581/1616-1628` —
a re-work storm analogous to the NAR storm, on the overlay path). Byte-parity floor
sacred (58 match); an eval-speed fix must never change a drvPath. This is task-4
(sui > nix efficiency) territory, same as the NAR-memo win.

## 1. The honest retention verdict

**The cid OOM (if real) is the INHERENT live working-set of computing a darwin-system
toplevel Merkle hash — thousands of derivation `Value`s that must all be live at once —
NOT a reclaimable retention leak. Eviction/GC will not fix it; a correct GC keeps every
node because every node is reachable from the root being hashed.** (verify verdict #1)

- **Dominant term (irreducible-to-eviction):** a parent `.drv` hash is a pure function
  of all input `.drv` hashes (Merkle DAG). Computing the toplevel hash forces its
  drvPath, which gathers `input_derivations` from string context
  (`derivation.rs:339-357`) — the transitive `.drv` closure. Every reachable derivation
  must exist simultaneously. Freeing a node before the top hash is computed is
  incorrect — it is still referenced. This is not garbage; it is the answer under
  construction.
- **Genuine over-retention exists but is a CONSTANT factor, not the cause:**
  `ThunkInner` double-stores every forced result — `OnceCell<Box<Concrete>>`
  (`value.rs:854-855`) **and** `ThunkRepr::Evaluated(Box<Value>)` (`value.rs:1211/1225`).
  Collapsing to one representation buys ~1.3–1.8× on value-node memory. **A reprieve,
  not a scaling fix** — the next-larger closure OOMs again. (The `Suspended{expr,env}`
  capture IS correctly dropped on force, `value.rs:1090` — not a classic leak.)
- **Three secondary amplifiers (reducible tier, not the dominant frontier):** the
  process-lifetime `IMPORT_CACHE` (`import_cache.rs:20`, never cleared, holds a fully-
  forced `Value` per imported file); `NixAttrs::Overlay` retaining both parents *after*
  caching the flattened map (`value.rs:1581`, ~2× per iterated overlay node across the
  50+-deep module-system fixpoint); the thunk double-store above. Bounding these helps
  the constant, not the O(closure-size) scaling.

**The ranking is a code-derived HYPOTHESIS, not measured bytes.** A `dhat`/massif
profile (or the `perf.rs` `ThunkForce`/`ImportHit`/`EnvClone` counters) at the OOM
point must confirm the dominant term before acting; reorder if an amplifier surprises.

## 2. The load-bearing fix — STREAMING, not eviction

The correct output is the top hash + the on-disk `.drv` files
(`write_derivation_to_store` already runs). The in-memory `Value` tree is **scaffolding
that can be torn down bottom-up as each node's hash finalizes.**

1. **Bottom-up hash-and-release (THE dominant-term fix).** As each derivation's ATerm is
   folded into its `.drv` hash and its string-context (`Output`/`DrvDeep` refs) is
   captured into the parent, **release the sub-`Value`** — keep only hash + context, drop
   the memo payload. Peak live = the hash-walk *frontier*, not the whole closure. This is
   *finalize-then-detach*, so there is nothing to recompute and nothing to diverge —
   which is why it is byte-safe where eviction-and-recompute is not.
2. **Collapse the thunk double-store (constant factor, do regardless).** ~1.3–1.8×;
   cheapest real win; may alone pull cid under the ceiling if it is marginally over.
3. **Externalize the memo to the CA store** (`sui-graph-store`, redb/rkyv/mmap) — forced
   sub-closures spill to disk-backed content-addressed storage, byte-neutral because
   keyed by a hash that already fixed the inputs.
4. **Bound the reducible caches** (`IMPORT_CACHE`, eval-memo, `nar_memo`) with LRU —
   evicting **only** content-addressed / pure-recompute entries, gated by the purity
   predicate (§3). Constant-factor tier.

No ML. Watermark hysteresis / a `MemoryBand` controller / W-TinyLFU / arena reclamation
are **deferred refinements**, not the fix.

## 3. The seal — a purity-admission gate, NOT blanket byte-neutrality

"Byte-neutral" is reserved for the content-addressed layer (a theorem) and provably-pure
subgraphs. The gate (reusing `sui-spec::laziness` `is_parity_capable`/`Tracked`/
`Referential`):

| Axis | Tier |
|---|---|
| Release/evict a **content-addressed** artifact (drvPath output, `source_hash+lock_hash` entry) | **truly byte-neutral (theorem)** — different recompute → different key → different line |
| NAR-memo / import entry over a **store path / retained on-disk source** | **byte-safe by construction** |
| Subgraph downstream of `currentTime`/`getEnv`/mutable `readFile`/`with`-taint | **UNSAFE — must HOLD** (`ca_key()==None`, never released-and-recomputed) |
| Streaming-release a `Value` still referenced by a live thunk | **only-mitigated** — release only when refcount confirms no live capturer |
| "streaming preserves drvPath" corpus-wide | **CI forcing-function** (Rust can't prove the graph-walk quantifier) |
| Hard peak cap under one giant `Value` | **NOT DESIGNED** — needs eval-time size rejection; out of scope |

**Streaming-release of the impure frontier is NOT byte-neutral — it is
hold-and-stream-through-once** (finalize the hash, capture context, drop the value; the
value is never *recomputed*). That distinction is load-bearing.

## 4. The deferred substrate (a named destination, NOT built now)

A tiered `MemoryPolicy : shikumi::TieredConfig` (bare ← discovered-RAM ← override), a
`RamDiscoveryLayer`, `MemoryBand`/watermark hysteresis, W-TinyLFU — **all deferred.**
They earn their keep only on demonstrated **third-use reuse** across sui runs (the org's
own over-abstraction test). The upgrade path so we don't paint into a corner: if a
bounded-cache tier ships, use a **flat `SuiMemoryCaps` struct** (default = today's
behavior) wired at both construction sites; lift it to `TieredConfig` only on the third
real reuse. **If and only if** that substrate is ever reached, mint **`hikae` (控え)**
("held in reserve / bounded") — Japanese-foundational, sibling of `atatame` (温め, the
*warm*-store: atatame keeps closures warm *across* runs, hikae bounds them *within* a
run). Not before.

## 5. Claims that must NOT round up (the caught overclaims — permanent record)

1. **"Eviction/GC fixes the cid OOM."** FALSE — the live set is fully reachable; only
   streaming shrinks the reachable frontier.
2. **"Every evicted unit is recompute-byte-identical."** OVERCLAIM — true only for the CA
   layer + pure subgraphs; false for the impure frontier. State the purity qualifier.
3. **"cid needs a config-tiered memory substrate."** FALSE/UNJUSTIFIED — the historical
   fix was ~55 lines of one derived option; even the general case wants streaming + a
   bounded cache, not a substrate. `hikae` is a destination gated on third-use reuse.
4. **The amplifier ranking is a code-derived HYPOTHESIS, not measured.** Profile first.
5. **The double-store collapse is a REPRIEVE (~1.3–1.8×), not a scaling fix.**
6. **"Streaming gives a hard peak cap."** NO — it shrinks the dominant term; a single
   giant `Value` is still unbounded (needs eval-time size rejection, out of scope).
7. **The 102/104 (now 58-match) parity floor proves only the NO-release path.** Byte-
   parity under streaming is unproven until the M0 differential gate is green.
8. **M0-A may moot all of this** — verify the cid failure is an RSS OOM-kill, not a
   force-throw, before building anything.

## 6. M0 (do these in order; each ships only if the prior didn't suffice)

- **M0-A — verify the failure mode** (`/usr/bin/time -l` peak RSS + `SUI_PERF_TRACE`):
  RSS OOM-kill vs force-throw vs slow-but-progressing. If not an RSS OOM, STOP — no
  memory work warranted.
- **M0-B (only if a real RSS OOM):** (1) ship the `drvPath-under-release` differential
  gate + `SUI_NO_STREAM_RELEASE=1` comparator FIRST (absent today; the 58-match floor
  proves only the no-release path); (2) land the thunk single-representation collapse
  (may alone suffice); (3) if still over, land bottom-up hash-and-release, byte-verified
  (hello + corpus) via the Parity Method before landing.
- **Done-predicate:** `nix eval .#darwinConfigurations.cid…toplevel.drvPath` (via sui)
  completes without OOM-kill AND the differential gate is green (drvPath unchanged under
  release across the corpus, `hello` byte-identical).
