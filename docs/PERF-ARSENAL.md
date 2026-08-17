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
- **S5 · list-concat structural-share** `[reuse-uniquely-owned-alloc]` — SHIPPED (this branch).
  `++` (`eval.rs:2583` → `value::concat_lists`) used `l.as_list()?.to_vec()` — an O(n) clone of the
  whole left accumulator per concat, O(n²) over a growing left-assoc `++` chain. Fix: `Rc::try_unwrap`
  the left backing `Vec` and **append the right in place** when the `Rc` is uniquely owned
  (`strong_count == 1`); clone-extend fallback (byte-identical to the old path) when shared. Enabler:
  `eval_binop` now `into_value()`s the operand `Concrete`s instead of `to_value()` (move, not
  clone-and-keep-alive), so a fresh `++` temporary's `Rc` reaches `concat_lists` with refcount 1.
  **PROVABLY-NEUTRAL** — both paths yield the identical ordered sequence of the same `Rc`-shared lazy
  `Value` thunks; **no element is forced, reordered, or re-identified** (proven by an `Rc::ptr_eq`
  element-identity unit test + the 4 sealed corpus rows). truly-unrep on the byte axis via `sui parity`.
  - **Measured (SUI_EVAL_PERF `list-concat` counter):** synthetic left-assoc `++` chain n=15000 →
    baseline copies **112,485,000** elements (Σ 1..14999), fix copies **0** (100% reuse). Real
    `hello.drvPath` tree-walker eval: 43,944 concat calls, **8.7% live reuse rate** (3,858 elems reused).
  - **Ceiling (NAMED): the wall-time win is negligible for small-element lists.** Nix list elements are
    `Value` (16 B — Int/Path/interned-String/`Rc`-thunk); `Vec::<Value>::clone` is a memcpy-fast,
    cache-friendly linear copy, so eliminating 112M *count* of copies does **not** move best-of-5
    wall-time at n≤15000 (0.33s → 0.33s). The win is **allocation + copy-count elimination**, not a
    measured speedup — the fold-heavy nixpkgs cost lives in `force`/`overlay`/`sorted_entries`, not
    `++`. The `foldl' (acc: x: acc ++ [x])` shape stays 0% reuse (the `acc` binding pins refcount ≥2);
    the fast path fires only for fresh `++` temporaries (chained literals + intermediate results) — the
    8.7% seen live. **Do not round the reuse-rate up to a speedup.**
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
- **C-filter · `filterAttrs` iter_unsorted** (`attrs.rs`) — SHIPPED (already `iter_unsorted`) +
  **VERIFIED + SEALED this branch (M1.5)**, stays NEEDS-VERIFICATION-with-argument. Both obligations
  discharged: **(a) no drvPath on the error path** — the builtin only ever builds a fresh plain
  attrset, constructs no derivation, and returns `Err` with no value on a predicate throw, so the
  byte-parity axis (drvPath) is unreachable when order could matter; **(b) no cross-entry force
  dependency** — the predicate is applied per entry as `apply(pred,k)` then `apply_and_force(_,v)`, a
  fresh partial over an immutable shared `pred`; one entry's force changes neither another's boolean
  outcome nor whether it throws. **★ SUPERSEDED 2026-08-17 — C-filter NO LONGER EXISTS.** The
  "load-bearing finding" recorded here was that there is NO parity corpus row and there cannot be,
  because `builtins.filterAttrs` is a sui-ONLY extension (`builtins ? filterAttrs` = false in nix)
  and nixpkgs `lib.filterAttrs` is `removeAttrs set (filter …)` (never routes through it). That is
  correct, and read once more it is a **defect report, not a caveat**: a builtin sui had and nix did
  not is sui accepting a program nix rejects. `filterAttrs` was removed from all three engines on
  2026-08-17 along with the other five nixpkgs-lib leaks, so the optimization this row adjudicated
  has no code path left. The `builtins/attrs.rs` unit tests that sealed it went with it. Nothing is
  left unsealed — an unreachable optimization needs no seal — and the residual below is moot for the
  same reason. `mapAttrs` carries the identical `iter_unsorted` shape and IS a real builtin; it is
  the one that matters now. **Residual, now historical (NOT rounded up):** under `iter_unsorted`
  WHICH throwing entry errored FIRST was hasher-seed-nondeterministic → the error *message* was
  order-sensitive (success-neutral, not error-deterministic). Byte-parity unaffected. It was
  NEEDS-VERIFICATION and was never promoted to PROVABLY-NEUTRAL.
- **C-A · attrs-eq-by-borrow** (`Concrete::eq` Attrs arm) — **SHIPPED + PROMOTED to PROVABLY-NEUTRAL
  this branch (M1.5).** `a.inner() == b.inner()` (which flattened AND cloned both backing FxHashMaps
  via `inner()` = `as_flat().clone()`) → `a.as_flat() == b.as_flat()` (borrow). The demand-order
  obligation is discharged, and stronger than the NEEDS-VERIFICATION prior suggested: **the clone
  touches NO `.demand()` site.** Cloning a `Value` is an `Rc`-bump that forces nothing; the two maps
  are compared by the *identical* `HashMap::eq` (same keys, same per-value `Value::eq` calls). The
  only `.demand()` in the arm are (1) the derivation short-circuit ABOVE the clone (unchanged) and (2)
  inside `Value::eq` — which **swallows force errors to `Null` (`unwrap_or`) and cannot throw**, so
  the Attrs-eq compare path has no error to reorder at all. Result is order-independent (HashMap
  equality). Sealed by 3 unit tests (incl. a shared-throwing-thunk no-force/no-throw test) + 2 parity
  corpus rows (attrset `==`/`elem` value surface + an `==`-selected derivation-arg **drvPath** byte
  check) + the `SUI_EVAL_PERF` `attrs-eq` counter (structural-eq calls + entries-clone-elided).

### RISKY / DEFAULT-NOT-NEUTRAL — the four "discovered candidates" (M2 adjudicated 2026-07-12)

**M2 verdict summary (worktree `m2-risky-tier`, base `bb47971`; byte oracle = the two
named drvPath probes, both byte-identical to nix on BOTH engines throughout — hello
x86_64-linux `j8q5j0x4…`, hello aarch64-darwin `a1fzz00d…`):**

> **The decisive structural fact all three share:** `NixAttrs` wraps `AttrsInner`, whose
> `Flat` variant holds an **`im_rc::HashMap` 15.1.0** (a persistent HAMT). So `(**attrs).clone()`
> / `(**la).clone()` is an **O(1) HAMT-root Arc-bump** (Overlay: 2×`Rc`+1×`Rc<OnceCell>`, also
> O(1)), NOT the O(n) deep copy the candidate descriptions assumed. The "waste" is a *count of
> O(1) clones*, not a copy volume — which changes the calculus decisively for C-with and C-slash.

- **C-with · with-scope cache clone** (`value.rs` `lookup_fast`/`lookup_sym`/`lookup_with_cache_only`)
  — **DEFERRED (measured-negligible + high-risk).** Measured over the full marquee eval with a
  `WithScopeCacheClone` counter: **93** clones (hello x86_64-linux) / **460** (hello aarch64-darwin),
  vs ~1.1M–1.9M `eval_expr` and 131k–286k forces. Each is an O(1) HAMT Arc-bump, so the *total*
  eliminable work is a few hundred Arc-bumps — no measurable win. Meanwhile the clone sits on the
  exact **M2.6 ROOT #4a** lazy-namespace force-order path. Sharing the `Rc` instead of cloning the
  map would risk that force-timing class for **zero** measured payoff. **Exact defer reason: the
  clone is already O(1) (`im_rc`) and fires ≤460× on the marquee eval — the win is negligible and
  cannot justify touching the ROOT #4a force-order class.** (The force decision happens *above* the
  clone — the scope is already forced to `Value::Attrs` before the cache clone runs — so the clone
  isn't itself force-order-sensitive; but there is nothing to gain by changing it.)
- **C-slash · per-`//` deferred-tail clone** (`eval.rs` `lazy_overlay_merge`) — **DEFERRED
  (zero measured waste on the marquee path).** A `SlashDeferredTailClone` counter measured
  **0** invocations on BOTH hello probes. `lazy_overlay_merge` is only reached on the
  deferred-**dynamic**-tail path (`o.${dynKey}.y = …`), which the hello/darwin stdenv evals never
  hit. There is no waste to eliminate on any corpus row, and the real `//` (`eval.rs:2577`) is
  already clone-free. **Exact defer reason: 0 invocations on the marquee eval → no measurable win;
  and eliminating the clone would change Overlay-node identity that `Concrete::eq`'s `Rc::ptr_eq`
  shortcut relies on (`value.rs:512/514`) — real risk for zero gain.**
- **C-store · thunk double-store** (`value.rs` `force_inner` Ok arm) — **PARTIALLY LANDED (the
  provably-neutral slice) + DEFERRED (the delicate slice).** The Ok arm has TWO store-points:
  Store#1 (pre-`while let Value::Thunk` unwrap loop) and Store#2 (post-loop). Measured: `ThunkStoreWrites`
  = 131,438 (linux) / 285,587 (darwin) forces. A `ThunkStoreRedundant` sub-probe showed that in
  **33% (linux) / 58% (darwin)** of forces `value` is NOT a thunk at Store#1, so the loop never runs
  and **Store#2 rewrites byte-identical repr content + re-attempts a no-op `OnceCell` `cache.set`** —
  a pure redundant store. **LANDED:** skip Store#2 in exactly this `!was_thunk_before_loop` branch
  (early-return with the identical `pop_force`/`trace_force_exit`/`Ok(value)` cleanup). **Per-site
  force-order/identity proof:** (a) Store#1 already established the terminal (`repr=Evaluated(value)`
  + guarded `cache.set`); (b) the body `evaluator(&expr,&env)` has RETURNED before the Ok arm, so no
  re-entrant force of `self` is in flight — the loop only `peek()`s OTHER thunks' OnceCell caches,
  never `self.0.repr`; (c) no code observes the `Box`'s pointer identity — `ThunkRepr::Evaluated` is
  read only by value (grep-confirmed: `value.rs:1621/1657`); (d) the string-context-carrying
  `Concrete::String` sits unchanged in repr (no re-derivation). Sealed by a regression test
  (`thunk_force_concrete_skips_redundant_store_but_caches`: value correct, `is_evaluated()`, `peek()`
  populated, re-force hits the OnceCell without re-eval) + both byte-oracle probes on both engines +
  the linux basket (11/11) + the darwin corpus (9/9). **Named wall-time CEILING:** `Value` is 16 B and
  `Box::new(value.clone())` is a small-alloc + Rc-bump; the 43k–165k eliminated allocations are an
  **allocation-count reduction, NOT a measured wall-time speedup** (marquee cost is overlay-flatten
  21.5% + force machinery, not thunk-store allocs; darwin best-of-3 is 7.7–8.8s, store-I/O-bound —
  a speedup claim would be noise). Tier: **PROVABLY-NEUTRAL on content+order** for this slice (proof
  above), byte-gated belt-and-suspenders because it edits `unsafe` force machinery.
  **DEFERRED — the full collapse:** the *other* 67%/42% of forces (loop ran / broke-early) are NOT
  redundant — a `ThunkStoreLoopMutated` probe showed **31%/19%** of forces have the loop collapse a
  thunk to a *different* concrete, so Store#1 and Store#2 hold different content there; collapsing
  those to a single store, or reordering the `unsafe` repr/cache writes across the Promise/Blackhole
  re-entrance machinery, stays RISKY (string-context + repr-visible-before-peek ordering rest on
  runtime invariants, not types → `only-mitigated` at best). **Exact defer reason for the full
  collapse: in 19–31% of forces the two stores hold genuinely different values (loop-collapsed), so
  a single-store collapse is not content-neutral; and the remaining reorder touches unsafe
  force-machinery whose neutrality is a runtime argument, not a type.**
- **C-eq-demand** — the `Concrete::eq` clone on its *demand* axis; bundle the demand-order proof with
  C-A (already PROVABLY-NEUTRAL, M1.5). No M2 action.

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
  `tatara-lisp/tatara-lisp-eval/src/interner.rs` Arc/Send+Sync). **M4 VERDICT (2026-07-12):
  DESIGN-DEFERRED, not extracted** — deferring-with-a-design is the honest outcome here, and a real
  deliverable (the org anti-premature-mint rule: a shared crate is earned-in-principle, but code-movement
  across two crates — one of them sui, whose adoption is byte-parity-critical — is heavy + byte-risky and
  must not be forced on a parity branch). The design is below; the trigger is named.

  **Why one handle cannot be forced (the four genuine divergence axes):**
  | axis | `sui-intern` | `tatara-lisp-eval` |
  |---|---|---|
  | handle | `Symbol(u32)` index into a `Vec<Rc<str>>` | `Arc<str>` — the pointer IS the handle |
  | equality | integer `==` on the `u32` | `Arc::ptr_eq` + byte fallback |
  | thread bound | `Rc` — `!Send` (single-threaded eval) | `Arc` — `Send + Sync` (cross-thread values stay valid) |
  | reverse resolution | needs a `Vec` reverse map (`resolve`/`resolve_rc`/`try_resolve`/`lookup`) + **prewarm-low-index + `Symbol: Ord`** (load-bearing for attrset-key byte-parity) | self-resolving (`Arc<str>` derefs to `&str`); no reverse map; `intern`/`clear`/`size` only |

  u32-only would break tatara's `Send + Sync`; Arc-only would cost sui its integer-eq **and** its
  `Symbol: Ord` prewarm-ordering (a byte-parity dependency). So the generic must abstract the handle,
  the equality strategy, the reverse-resolution presence, and the thread bound — 4 axes, not a
  find-and-replace.

  **The `InternHandle` design (the M4 deliverable):**
  ```rust
  /// A cheap, comparable interned-string handle. The interner is generic
  /// over this trait so sui (u32/Rc/ordered) and tatara (Arc/Send+Sync)
  /// share ONE table + prewarm engine without forcing one handle on the other.
  trait InternHandle: Clone + Eq {
      /// The stored form the handle resolves against (`Rc<str>` / `Arc<str>`).
      type Backing: Clone;
      /// Mint the Nth handle when a new string is inserted (u32 index vs Arc-clone).
      fn from_insert(index: usize, backing: &Self::Backing) -> Self;
      /// Resolve to the shared backing (Vec lookup vs identity).
      fn resolve(&self, table: &InternTable<Self>) -> Self::Backing;
  }
  struct Interner<H: InternHandle> { /* shared FxHashMap<H::Backing, H> + reverse + prewarm hook */ }
  ```
  Ordering (`Symbol: Ord`) is an **`H`-specific** capability, exposed only where the backing supports it
  (a `trait OrderedHandle: InternHandle` sui implements and tatara does not) — so the generic never
  imposes an order tatara can't provide, and never drops the order sui's byte-parity needs.

  **Adoption asymmetry (why "byte-neutral" is non-transferable):** sui's migration onto `Interner<Symbol>`
  is **byte-verified** (GetAttr/MakeAttrs/UpdateAttrs are parity-critical — a full `sui parity` gate on
  the migration commit); tatara's onto `Interner<ArcHandle>` is **semantics-verified only** (unit tests,
  no byte oracle). "Byte-neutral" is a sui-path label and does not carry to tatara.

  **`pending-interner:` trigger to promote from design → extraction:** land the `InternHandle` +
  `OrderedHandle` traits in `sui-intern` behind a feature; migrate sui's `Interner`/`Symbol` onto it
  **byte-verified** (`sui parity` green on the migration commit, GetAttr/MakeAttrs unchanged); then a
  **second repo** (tatara) adopts `Interner<ArcHandle>` semantics-verified. Only when the *second repo
  forces the shared shape* does the crate leave `sui-intern` for shared substrate (the ≥3-use / 2nd-repo
  bar). Not copy-paste-today: the sui side is byte-risky and the shape must be exercised by a real 2nd
  consumer before a mint.
- **`unsorted-iter` is NOT a fleet pattern** — a single method, not a recurring cross-site shape.

## 4. The `/algorithmic-prowess-seal` fold — `sealed-optimization` family

The catalog is a `(defseal)` family graded on two axes (value · structural), headline = `min`, and
`selo::SealTier{TrulyUnrep | ParseTimeRejected | OnlyMitigated(Ceiling)}` **requires the ceiling** →
a ceiling-less byte-neutrality claim is **unrepresentable in the catalog**. That's the honesty gate.

**Today `ByteNeutral` is a COMMENT, not a TYPE** (grep-confirmed: no `enum ByteNeutral`, no `trait
PureFn`; `iter()`/`iter_unsorted()` are type-indistinguishable). Two type-moves earn their keep:
- **`ContentKey<T>` (M3) — SHIPPED this branch (`sui-intern/src/memo.rs`).** The `ContentKey<T>` newtype
  has a **sole constructor** `ContentKey::of(&T) -> ContentKey<T>` that hashes `T`'s structural read-set
  (everything its `Hash` impl writes) through a BLAKE3-backed `std::hash::Hasher` → a 32-byte content
  digest, private fields, `PhantomData<fn() -> T>` for type-safety. The keyed memo API
  `ContentMemo<ContentKey<T>, V>::get_or_compute_keyed(&T, impl FnOnce(&T) -> V)` derives the key from
  the SAME `&T` it hands to `compute`, so a key decoupled from the computed input **has no constructor**.
  The general `get_or_compute` (documented only-mitigated) is kept for stable-key callers with no `T`.
  **Tier — graded on two axes, exactly (no round-up):**
  - **KEY↔CONTENT structural axis → PARSE-TIME-REJECTED.** There is no expressible program that memoizes
    under a key not derived from the computed input: `of(&T)` is the only way to build a `ContentKey<T>`,
    fields are private (proven by a `compile_fail` doctest forging raw digest bytes), and passing a `&B`
    where a `&A`-key is expected is a type error (second `compile_fail` doctest). The `stale-key/decoupling`
    footgun (the `Sharing::PerSite`/libxcrypt divergence class) is structurally unrepresentable *on this
    axis*.
  - **PURITY axis (`T → V`) → ONLY-MITIGATED (C1) FOREVER.** `ContentKey<T>` seals *decoupling*, NOT
    *purity*: `compute` could still read wall-clock / `getEnv` / a mutable FS — opaque to the type
    system. There is no `PureFn` in safe Rust and there cannot be; promising one is a fantasy this plan
    refuses. The module invariant + the CI byte gate are the correct terminal C2 enforcement. **Do not
    read `ContentKey<T>` as a purity proof — it is a decoupling proof.**

  Sealed by: `content_key_of_is_deterministic`, `content_key_differs_on_different_content`,
  `keyed_memo_round_trips_and_does_not_recompute_on_hit` (hit on a structurally-equal but *distinct*
  object → the key is the content, not object identity), + the two `compile_fail` type-seal doctests.
  **Zero behavior change to any existing memo** — additive API on a module with 0 live consumers, so the
  byte-parity axis is provably untouched (no drvPath reachable from `sui-intern::memo`).
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
   precedent; below the ≥3 bar. Only the interner is a real duplication (**M4: design-deferred**, §3 —
   the design + `pending-interner:` trigger recorded, no code moved).
3. **"The invariants can be Rust types."** MOSTLY still true (do not over-correct): **purity is opaque to
   the type system (C1 forever)** and the two iterators are type-indistinguishable — so most axes stay
   only-mitigated. But **M3 shipped ONE genuine parse-time-rejected type seal** — `ContentKey<T>`'s
   key↔content *decoupling* axis (sole `of(&T)` constructor + private fields; `compile_fail`-proven).
   That is a *decoupling* seal, NOT a *purity* seal — the value's byte-neutrality still rests on
   content-addressing's *math* + the CI byte gate (C2), not on `rustc`. So: one parse-time-rejected
   type-level seal (the decoupling axis) + one theorem tier (content-addressing) + everything else
   only-mitigated. The old "ZERO type-level unrep seals" line is superseded by M3 — but the purity
   fantasy stays refused.
4. **"Add persistent HAMT bindings."** CLOSED premise — sui already ships `im_rc::HashMap` + Rc sharing
   + COW. The live gaps are elsewhere: LIST `++` clones `Rc<Vec>` O(n) — **now structural-shared (S5):
   `Rc::try_unwrap` + in-place extend for a uniquely-owned left, byte-neutral, 100% reuse on fresh-`++`
   chains but 8.7% live + a NAMED wall-time ceiling for small-element lists** (the full RRB-tree/`im::Vector`
   swap stays REJECTED — a representation change of the borrowing `as_list() -> &[Value]` surface, high
   byte-risk, and the memcpy-fast small-element copy makes the wall-time payoff unproven); the `//`
   deferred-tail deep-copy (C-slash, RISKY), small-attrs inline storage — not bindings.
5. **The CI gate covers the marquee eval.** FALSE — ~8 shapes, partial corpus.

## 6. Phased plan

- **M0** — register S1–S5 as `(defseal)` entries at their honest tier + land the honesty gate. ContentMemo
  stays sui-local (record the promote trigger). **S5 (list-concat structural-share) SHIPPED this branch**
  at PROVABLY-NEUTRAL with a NAMED wall-time ceiling + 4 sealed corpus rows + the `SUI_EVAL_PERF`
  `list-concat` counter (the durable measurement tool).
- **M1** — the PROVABLY-NEUTRAL one-liners (count-not-clone, map-builder family), each its own commit + corpus confirm.
- **M1.5** — NEEDS-VERIFICATION (C-A, C-filter): success-path argument + explicit nix diff. **SHIPPED
  this branch.** C-A: `a.inner()==b.inner()` → `a.as_flat()==b.as_flat()`, demand-order obligation
  discharged (the arm forces nothing in the compare path; `Value::eq` swallows to `Null`) →
  **PROMOTED to PROVABLY-NEUTRAL**; sealed by 3 unit tests + 2 parity corpus rows (incl. an
  `==`-selected drvPath) + a `SUI_EVAL_PERF attrs-eq` counter. C-filter: **RETIRED 2026-08-17 —
  `builtins.filterAttrs` was removed as a nixpkgs-lib leak, so the arm it optimized is gone.** It had
  been NEEDS-VERIFICATION-with-argument (error-message order residual) with NO corpus row possible,
  and *that* — sui-only + off the marquee path — was the leak announcing itself as a perf caveat.
- **M2** — RISKY arms adjudicated (2026-07-12; see §2 RISKY block for the measured detail):
  **C-with DEFERRED** (≤460 O(1) HAMT clones on the marquee eval — negligible + ROOT #4a
  force-order risk); **C-slash DEFERRED** (0 invocations on the marquee — zero waste + `Rc::ptr_eq`
  identity risk); **C-store PARTIALLY LANDED** — the provably-neutral redundant-Store#2 skip
  (33–58% of second-stores, byte-verified on both engines + linux basket 11/11 + darwin corpus 9/9,
  regression-test-sealed, wall-time ceiling NAMED), the full collapse DEFERRED (19–31% of forces are
  loop-collapsed → not content-neutral; unsafe-reorder neutrality is a runtime argument, not a type).
  **A rigorous DEFER is a real deliverable here — two of three arms defer with measured, exact reasons;
  no landing was forced.**
- **M3** — `ContentKey<T>` type-move — **SHIPPED this branch** (`sui-intern/src/memo.rs`). The
  key↔content decoupling axis → **parse-time-rejected** (sole `of(&T)` constructor + private fields,
  2 `compile_fail` type-seal doctests); the purity axis **stays only-mitigated C1 forever** (no `PureFn`
  in safe Rust). Keyed memo API `get_or_compute_keyed(&T, |&T| -> V)` added; general `get_or_compute`
  kept for stable-key callers. 3 new unit tests + 2 doctests; additive-only (0 live consumers) so
  byte-parity is provably untouched.
- **M4** — interner generic-over-handle — **DESIGN-DEFERRED this branch (a real deliverable, not a
  punt).** The `InternHandle`/`OrderedHandle` design + the four-axis divergence table + the
  `pending-interner:` promote-trigger are recorded in §3; **no code moved** (byte-risky on the
  parity-critical sui side, and the shape isn't yet forced by a 2nd repo — below the mint bar). Extract
  only when the trigger fires (sui migrates byte-verified, then tatara adopts semantics-verified).

## 7. REJECTED (byte-parity sacred)

`Env::child` HAMT-swap (premise closed; would change iteration order) · cross-call `referenced_idents`
memo (ephemeral-key collision) · naive hash-consing of unforced thunks (breaks blackhole identity) ·
per-file arena for the escaping value graph (unsound) · any `iter_unsorted` at an order-OBSERVED sink ·
the four RISKY candidates presented as neutral one-liners · any "fleet-wide byte-neutral" brand
(sui-path-specific label) · counting `iter_unsorted` as a fleet pattern · **the full `Rc<Vec<Value>>` →
`im::Vector`/RRB-tree list-representation swap** (S5 took the surgical in-place reuse instead — the swap
touches every `as_list() -> &[Value]` borrowing site (a representation change, byte-high-risk) AND the
small-element memcpy-fast copy leaves its wall-time payoff unproven; revisit only if a measured
large-element `++`-fold storm appears) · **presenting S5's reuse-rate as a wall-time speedup** (it is a
copy-count elimination with a named negligible-wall-time ceiling for small elements).

**Doctrine name: NONE** — not earned. The home is this graded `(defseal)` catalog in
`/algorithmic-prowess-seal` + the one shipped Rust primitive `sui-intern::ContentMemo` (sui-local).
Mint a name only if a second repo forces the interner/memo shape into shared substrate (the M4 trigger).
