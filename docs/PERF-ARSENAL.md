# The Byte-Neutral Perf Arsenal — a GRADED, sealed-optimization catalog

> Big-bang recon + adversarial verify (2026-07-12) against `sui` main @ `40ab4bc`/`dd38bc2`.
> **Three overclaims caught by the adversarial pass (two of them the author's) are recorded
> in §5.** Not a doctrine, not a `(defmemo)` macro, not a fleet "byte-neutral" brand — a
> tier-honest catalog of result-identical optimizations, each carrying its own neutrality
> invariant + named ceiling, folded into `/algorithmic-prowess-seal` as a `sealed-optimization`
> family. **The load-bearing rule: an `only-mitigated` entry is unrepresentable in the catalog
> without a named ceiling** (the honesty gate).

## 1. The bright line — neutrality is THREE tiers, not one

For a *lazy* evaluator, "result-neutral" is **observational equivalence**: identical output
bytes **AND** identical termination/error behavior **AND** identical thunk-forcing side effects
(string-context propagation, IFD triggering, demand order). That third clause is the sui trap.

| Tier | Mechanism | May ship behind the byte gate without a per-site nix diff? |
|---|---|---|
| **PROVABLY-NEUTRAL** | algebraic rewrite over unordered storage · pure syntactic predicate · clone→borrow of a count | **YES** (byte gate = belt-and-suspenders) |
| **NEEDS-PER-SITE-VERIFICATION** | eager eval inside an order-flipped loop — success-neutral but not error-neutral | **NO** — success-path only; needs an error-order argument + nix diff |
| **RISKY / DEFAULT-NOT-NEUTRAL** | touches WHEN/WHETHER a thunk forces, string-context, or attr identity | **NO** — needs a per-site force-order/context proof PLUS the Parity Method |

**Presenting a NEEDS-VERIFICATION or RISKY item as PROVABLY-NEUTRAL is the round-up this
catalog exists to forbid.**

## 2. The catalog (weakest axis wins; file:line vs live HEAD)

### PROVABLY-NEUTRAL — the founding tier
- **S1 · sort-storm-elimination** `[drop-unobserved-order]` — SHIPPED (`6d3226c`). **Measured 24–33%.**
  `iter_unsorted` at fresh-map sites; `AttrsInner::Flat` is unordered `im_rc::HashMap`, every
  observation re-sorts via `sorted_entries()` → the sort was dead work. truly-unrep (value).
- **S2 · recursion-detect-hoist** `[hoist-loop-invariant]` — SHIPPED (`a3ad95a`, "Storm A"). **O(N²)→O(N).**
  Pure syntactic AST predicate, same `NODE_ATTRPATH` exclusion; verdict-unchanged → truly-unrep both axes.
  *Self-declined:* cross-call memoizing `referenced_idents` on ephemeral `(source-id,range)` collides
  without a per-eval clear → byte-wrong. Correctly declined.
- **count-not-clone** (`value.rs:1755`, `NixAttrs::len`) — `as_flat().clone().len()` → `as_flat().len()`.
  A count needs no materialization. truly-unrep. (DESIGN, M1.)
- **order-agnostic map-builders** — intersectAttrs/mapAttrs(lazy-thunk values)/zipAttrsWith(→BTreeMap)/
  derivation-env(→BTreeMap)/lazy_overlay_merge/merge_nested_insert(unique keys). truly-unrep. (DESIGN, M1.)

### NEEDS-PER-SITE-VERIFICATION — success-neutral only
- **C-filter · `filterAttrs` iter_unsorted** (`attrs.rs:71-74`) — the one iter_unsorted site that is
  NOT clean: **eagerly forces the predicate in the loop**, so which throwing key errors first is
  hasher-seed-nondeterministic. Byte-neutral ONLY on the success path (no drvPath exists on error);
  not error-deterministic. Ship only with the "no-drvPath-on-error + no-cross-entry-force-dep" argument + nix diff.
- **C-A · attrs-eq-by-borrow** (`Concrete::eq` Attrs arm) — eq forces `type`/`outPath` via `.demand()`,
  so clone-elision could change demand-order-observable throws. Needs a demand-order argument + nix diff.

### RISKY / DEFAULT-NOT-NEUTRAL — the four "discovered candidates" (all touch force-order/identity)
- **C-with · with-scope clone** (`value.rs:1961-1975`) — the exact **M2.6 ROOT #4a** lazy-namespace
  force-order path; materializing the scope earlier can re-throw the `null`/`concatLists null` class. RISKY.
- **C-slash · per-`//` deferred-tail clone** (`eval.rs:2288/2308`) — the *real* `//` (`eval.rs:2577`) is
  already clone-free (Rc-shared lazy Overlay); this is the deferred-dynamic-tail path, and sharing it
  would change Overlay-node identity that `Concrete::eq`'s `Rc::ptr_eq` shortcut relies on. RISKY.
- **C-store · thunk double-store** (`value.rs:1211-1228`) — `EVAL-PERF-SEAL.md §7` itself defers it;
  `unsafe` force reorder + string-context propagation. RISKY (most delicate).
- **C-eq-demand** — the `Concrete::eq` clone on its *demand* axis; bundle the demand-order proof with C-A.

### Class B — ONLY-MITIGATED (memoization; ceiling NAMED)
- **S3 · ContentMemo** (`sui-intern/src/memo.rs`) — SHIPPED but **ZERO live consumers** (grep-confirmed;
  `eval.rs:589` explicitly *reserves* it; the real O(N²)→O(N) win came from the single-walk, not the memo).
  Purity is caller-discharged → **only-mitigated C1** (no dependent types) **+ C2** (external reads).
- **S4 · nar-hash-source-tree memo** (`sui-compat/src/source.rs`, `4b8f63b`) — SHIPPED as a separate
  hand-rolled map (NOT via ContentMemo). **only-mitigated C2** (tree immutable mid-eval).

## 3. Fleet integrability — earned vs speculative (verdict-corrected)

- **ContentMemo: STAY sui-local — NOT fleet-earned.** 0 live consumers; the three cited "hand-rolled
  precedents" did not adopt it (referenced_idents deliberately un-memoized; overlay-flatten is a
  different per-node `OnceCell`; only the NAR memo is same-shape and *still* hand-rolls) = **1 real
  same-shape use, below the ≥3 bar.** Trigger to promote: migrate the NAR memo onto it (use #1), find a
  genuine 2nd stable-content-key site, then a *second repo* forcing the shape.
- **The one real ≥2-crate duplication is the string interner** (`sui-intern` u32/Rc vs
  `tatara-lisp-eval/src/interner.rs` Arc/Send+Sync). Redistribution = **generic-over-handle**
  (`trait InternHandle`), NOT lowest-common-denominator; sui's adoption is *byte-verified*,
  tatara's is *semantics-verified only* ("byte-neutral" is a sui-path label, non-transferable).
  Earned-in-principle / DESTINATION — needs the `InternHandle` design pass (M4), not copy-paste-today.
- **`unsorted-iter` is NOT a fleet pattern** — a single method, not a recurring cross-site shape.

## 4. The `/algorithmic-prowess-seal` fold — `sealed-optimization` family

The catalog is a `(defseal)` family graded on two axes (value · structural), headline = `min`, and
`selo::SealTier{TrulyUnrep | ParseTimeRejected | OnlyMitigated(Ceiling)}` **requires the ceiling** →
a ceiling-less byte-neutrality claim is **unrepresentable in the catalog**. That's the honesty gate.

**Today `ByteNeutral` is a COMMENT, not a TYPE** (grep-confirmed: no `enum ByteNeutral`, no `trait
PureFn`; `iter()`/`iter_unsorted()` are type-indistinguishable). Two type-moves earn their keep:
- **`ContentKey<T>` (M3):** memo constructible only via a `ContentKey::of(&T)=blake3(read-set)` newtype
  whose sole constructor already fixed all inputs → the *stale-key/decoupling* footgun becomes
  parse-time-rejected. **Purity of `T→V` stays C1-ceiling-bound forever** — there is no `PureFn` in safe
  Rust and there cannot be; promising one is a fantasy this plan refuses. The CI byte gate is the
  correct terminal C2 enforcement.
- **`UnorderedIter` marker (deferred):** `iter_unsorted()` returns a wrapper with no exposed order →
  observing order without `.sorted()` is `E0599`; promotes `drop-unobserved-order`'s observation axis
  only-mitigated → parse-time-rejected. Build it when a 2nd order-observing consumer appears.

**Fleet enforcement today = the CI double-gate:** `parity.yml` (byte, C2, fired after the value exists) +
`perf-seal.yml` (deterministic op-count ±15%). **Honest scope limit: covers ~8 micro/storm shapes, NOT
the ~40-min marquee cid cost.** "Boxed provably" is true over a *partial* corpus. Do not round up.

## 5. Claims that must NOT round up (the caught overclaims)

1. **"Each cataloged primitive is byte-neutral."** OVERCLAIM — three tiers; the four RISKY candidates
   (C-with/C-slash/C-store/C-eq-demand) touch force-order/context/identity and are NOT proven neutral;
   C-filter is success-path-only.
2. **"Fleet integrability is earned."** OVERCLAIM — ContentMemo has **0 live consumers**, 1 same-shape
   precedent; below the ≥3 bar. Only the interner is a real duplication (design-gated).
3. **"The invariants can be Rust types."** FALSE — purity is opaque to the type system (C1 forever);
   the two iterators are type-indistinguishable. Mostly only-mitigated + one theorem tier
   (content-addressing, sealed by the key's *math*, not `rustc`). ZERO type-level unrep seals today.
4. **"Add persistent HAMT bindings."** CLOSED premise — sui already ships `im_rc::HashMap` + Rc sharing
   + COW. The live gaps are elsewhere: LIST `++` clones `Rc<Vec>` O(n) (RRB-tree = the real move),
   the `//` deferred-tail deep-copy (C-slash, RISKY), small-attrs inline storage — not bindings.
5. **The CI gate covers the marquee eval.** FALSE — ~8 shapes, partial corpus.

## 6. Phased plan

- **M0** — register S1–S4 as `(defseal)` entries at their honest tier + land the honesty gate. ContentMemo
  stays sui-local (record the promote trigger).
- **M1** — the PROVABLY-NEUTRAL one-liners (count-not-clone, map-builder family), each its own commit + corpus confirm.
- **M1.5** — NEEDS-VERIFICATION (C-A, C-filter): success-path argument + explicit nix diff.
- **M2** — RISKY arms (C-with → C-slash → C-store → C-eq-demand): per-site force-order proof + Parity Method,
  or stay REJECTED.
- **M3** — `ContentKey<T>` type-move (stale-key axis → parse-rejected; purity stays C1).
- **M4** — interner generic-over-handle crate (design-gated on `InternHandle`).

## 7. REJECTED (byte-parity sacred)

`Env::child` HAMT-swap (premise closed; would change iteration order) · cross-call `referenced_idents`
memo (ephemeral-key collision) · naive hash-consing of unforced thunks (breaks blackhole identity) ·
per-file arena for the escaping value graph (unsound) · any `iter_unsorted` at an order-OBSERVED sink ·
the four RISKY candidates presented as neutral one-liners · any "fleet-wide byte-neutral" brand
(sui-path-specific label) · counting `iter_unsorted` as a fleet pattern.

**Doctrine name: NONE** — not earned. The home is this graded `(defseal)` catalog in
`/algorithmic-prowess-seal` + the one shipped Rust primitive `sui-intern::ContentMemo` (sui-local).
Mint a name only if a second repo forces the interner/memo shape into shared substrate (the M4 trigger).
