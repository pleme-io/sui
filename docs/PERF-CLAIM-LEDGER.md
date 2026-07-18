# The perf-lever claim ledger (`sui-spec::perf`)

A typed, honesty-gated **claim ledger** over every optimization applied to
sui's eval hot path. It is sui-spec's 23rd TYPED-SPEC domain — a border
(`sui-spec/src/perf.rs`) + a canonical Lisp corpus
(`sui-spec/specs/perf.lisp`) + an interpreter + a catalog row — authored,
catalogued, and tested like `laziness` and `coercion`.

**It is not a doctrine and not an optimization engine.** It types the *claim
about* a perf lever; it does not generate optimizations and does not verify
that an edit is byte-parity-sound. This doc is the M0 record + the honest
verdict on what the ledger does and does not catch.

## Why it exists

A sui perf session produces *levers* — a NanBox repr swap, a lexical
pre-resolver, a positional-frame overlay, a redundant-store elision. Each is a
**claim**: "this made eval faster and stayed byte-identical to nix." Those
claims used to live only in commit messages, [`PERF-ARSENAL.md`](./PERF-ARSENAL.md),
and a session summary — and in one session two claim-mistakes slipped through
by hand:

- a lever that **measured null** was nearly recorded as a win
  (`m0-resolver`: `SUI_RESOLVE=1` measured −0.5 % ≈ null);
- a lever that **measured net-negative** was proposed as shippable
  (`positional-frames`: +7 % fib / +32–39 % call-heavy), and its technique —
  a resolution change — was described with a proof-tier stronger than a partial
  byte corpus can honestly earn.

The ledger makes those two mistake-classes a property of the *claim*, not of
the author's diligence.

## What it catches — the honest verdict

Scored against the four real mistakes of the session that produced it:

| # | Mistake | Caught? | Mechanism | Tier |
|---|---|---|---|---|
| c | null measurement recorded as a win | **yes, mechanically** | `is_honest`: `Proven ⇒ measured Improved`; `Improved ⇒ speedup_bp > 0` | eval-caught |
| d₁ | a regression proposed as a win | **yes** | `Delta::measured(before,after)` returns `None` when `after ≥ before` — no `Delta` value names a non-positive speedup | truly-unrepresentable **on the sign axis** |
| d₂ | a resolution change claiming byte-sufficiency | **yes, once labeled right** | `is_honest`: `claimed_tier ≤ earned_tier(technique)`; `earned_tier(ResolutionChange) = Rejected` | eval-caught |
| a | a number measured on one engine attributed to another | **no** | lives in harness wiring, outside the claim's typed shape | — |
| b | re-claiming an already-landed lever | **no (as designed)** | needs a cross-lever "no two Landed levers share a primary counter" invariant | — (named M2) |

**1 fully mechanical, 1 partial, 2 judgment.** This is not ceremony — it kills
the null and negative/tier-contradiction classes that actually bit. It is also
**not** "every future lever is safe by construction": that sell would be the
exact round-up the ledger exists to refuse. The type gates the claim **shape**;
it does not verify soundness, and it does not reach mistakes (a) and (b).

### The load-bearing escape hatch (stated, not hidden)

`earned_tier` maps a **technique class** to the strongest honest proof-tier. A
lever whose `:technique` is *mislabeled* — a force-order change called
`ReprSwap` — passes `is_honest` while being byte-unsound. The type gates the
class; the truth of the label, and the actual parity of the edit, stay the
`sui parity` byte oracle's job over a **partial** corpus (C2, external
observation, forever). Every `:proof-tier ByteSufficient` carries this caveat.

## The border (M0)

```lisp
(defperf-lever
  :name       "nanbox-upvalues"
  :attacks    ("vm-closure-upvalue-clone" "thunk-upvalue-clone")
  :technique  ReprSwap          ; ReprSwap | DropUnobservedOrder | SkipRedundantStore
                                ; | HoistInvariant | ForceOrderChange | ResolutionChange
  :proof-tier ByteSufficient    ; Rejected < ForceOrderProof < CouplingProof < ByteSufficient
  :status     Landed            ; Proposed | Landed | Proven | Discarded | Deferred
  :measured   Pending           ; Pending | Improved | NoImprovement | Regressed
  :speedup-bp 0                 ; basis points faster (>0 iff measured Improved)
  :ceiling    NotApplicable)    ; NotApplicable | PartialCorpus | PersistentLazyDesign | NoPureFn | ExternalObservation
```

- **`Delta`** — a strictly-positive speedup in basis points (`before/after − 1`,
  so 2× = 10 000 bp, unbounded above). Sole fallible ctors, private field,
  modeled on `sui_intern::memo::ContentKey`'s seal. **No `Delta` value names a
  null or a regression** — never-ship-a-regression made truly-unrepresentable
  on the sign axis. `MeasuredKind::Improved`'s delta is derived through it, so
  an "improved but actually slower" lever is unconstructable.
- **`earned_tier(technique)`** — the technique→proof-tier honesty table (the one
  net-new hand-authored judgment; soundness escapes the type here).
- **`is_honest` / `honesty_violation`** — the eval-caught red-flag predicate,
  the exact shape of `laziness::ThunkDiscipline::is_correctly_classified`.
- **`apply<E: PerfEnvironment>`** — audits a lever against a (mockable) coarse
  cost reading, returning a typed `SpecError::Interp { phase: "honesty" }` on a
  dishonest claim (never a silent Ok). The per-lever measure-**gate** (a reading
  vs a per-counter budget) is M1, not M0.

The M0 corpus is this session's **four real levers**, each in its honest record.
`every_authored_lever_is_honest` going green **is** the mechanical proof that
the null (`m0-resolver`) and the tier-contradiction (`positional-frames`) were
driven to their honest records, not rounded up. The mistake *forms* are
exercised as unit tests (`null_measurement_recorded_as_proven_is_caught`,
`a_resolution_change_claiming_byte_sufficiency_is_caught`,
`a_regression_has_no_delta_inhabitant`) — never authored into the corpus, exactly
as `laziness.lisp` authors only correctly-classified disciplines.

## Tier ledger

| Piece | Tier | Milestone |
|---|---|---|
| `defperf-lever` border + closed enums | parse-time-rejected (serde closed enum) | **M0 shipped** |
| `Delta` positive-only seal (no non-positive inhabitant) | **truly-unrepresentable** (sign axis) — provenance stays CI-caught | **M0 shipped** |
| `is_honest` + `every_lever_is_honest` catalog test | eval-caught / CI-caught | **M0 shipped** |
| `earned_tier(technique)` table | net-new hand-authored judgment (only-documented soundness; the mislabel escape hatch) | **M0 shipped, honestly bounded** |
| catalog registration (`every_authored_domain_is_in_catalog`) | CI-caught (the one unroundable gate) | **M0 shipped** |
| claim-CONTENT soundness (is the edit force-order-neutral) | C2 CI-caught forever via `sui parity` over a partial corpus | ceiling |
| per-lever measure-gate (`perf_seal` `Budget` per-counter, not `{eval_expr}`) | net-new | **M1** |
| `defcost-site` over the `sui_eval::perf::Counter` registry | parse-rejected + CI drift-test | **M1** |
| already-landed / mis-attribution invariant (mistake b, + optional `:engine`) | CI-caught | **M2** |
| `defalloc-site` (lifetime-legality) / `defstream` (byte-identity) | border-only, `M3TypedOnly` interpreter — gated on a real consumer | **M3** |

## Phased path

- **M0 — the claim-honesty ledger (shipped).** One form, one honesty predicate,
  this session's four levers, the three catalog edits. No `Budget`
  generalization, no live per-lever measure-gate — the ledger + the existing
  coarse `EvalExpr` gate (`perf_seal`) unchanged.
- **M1 — `defcost-site` + a real measure-gate.** A typed border over the
  56-variant `sui_eval::perf::Counter` enum (`Weight::Declared` honesty marker
  for the 53 prose-weighted counters); generalize `perf_seal`'s
  `Budget{eval_expr}` → per-counter, so a lever's *attacked* counter regressing
  trips the gate, not just the coarse aggregate.
- **M2 — the already-landed / mis-attribution invariant.** A catalog invariant
  `no_two_landed_levers_share_a_primary_counter` (closes mistake b), plus an
  optional `:engine {TreeWalker|Vm}` field bound to the harness invocation
  (narrows mistake a from pure-judgment toward parse-checkable).
- **M3 — border-only `defalloc-site` + `defstream`.** Author the borders with
  `M3TypedOnly` interpreters that return typed `SpecError::Interp` on the
  not-yet-driven paths (never a stub-Ok). Land only when a real repr-refactor
  (alloc) or a chunked-vs-buffered differential (stream) exists to drive them —
  the third-use test is currently **unmet** for both.

## Open risks (never round up)

- Mistakes (a) wrong-engine and (b) already-landed are **not** caught at M0.
  Selling the ledger as catching mis-attribution would itself be a round-up.
- `earned_tier` is hand-authored judgment; a mislabeled `:technique` passes the
  gate while being unsound. Soundness is C2-oracle-forever.
- The per-lever measure-gate M1 leans on is **unbuilt** —
  `perf_seal.rs`'s `Budget{eval_expr}` is single-field today. "M0 drives a real
  measure-gate" is true only for the coarse `EvalExpr` gate.
- `defalloc-site` + `defstream` have **zero consumer code** today; landing their
  interpreters early would be the forbidden stub-Ok. Border-only at M3.
- `:weight` is genuinely measured for only 3 of 56 counters (the `trace.rs`
  nanos accumulators); a cost model that reads as measured but is 53/56 prose is
  a partial model, and the `Weight::Declared` marker must stay load-bearing.

## Naming

No doctrine name, deliberately. [`PERF-ARSENAL.md`](./PERF-ARSENAL.md) §6/§7
already refused one ("Doctrine name: NONE — not earned"); minting one here would
repeat that overclaim. The module is `perf`; the keyword is `defperf-lever`;
the value is a typed claim-ledger, and naming a ledger a doctrine is the
round-up the ledger exists to refuse.

## References

- [`PERF-ARSENAL.md`](./PERF-ARSENAL.md) — the prose lever catalogue this ledger types.
- [`ENV-RESOLVE-DESIGN.md`](./ENV-RESOLVE-DESIGN.md) — the M0/M1/M2 resolver verdicts (§6a–c) whose null + negative measurements are two of the four M0 levers.
- [`EVAL-PERF-SEAL.md`](./EVAL-PERF-SEAL.md) — the coarse `EvalExpr` CI gate the per-lever measure-gate (M1) generalizes.
- `sui-spec/src/laziness.rs` — the TYPED-SPEC triplet template + the `is_correctly_classified` red-flag idiom.
- `sui-intern/src/memo.rs` — `ContentKey`'s sole-ctor seal, the `Delta` template.
