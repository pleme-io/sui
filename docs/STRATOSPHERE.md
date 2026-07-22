# STRATOSPHERE — the every-nix-use-case destination-first roadmap

> Operator goal (verbatim): *"take every single possible use case nix can have and
> test it with sui and ensure sui has clear performance, clarity, beauty, and even
> if required optimization that only tatara-lisp can give for runtime fluidity in
> order to shoot its performance into the stratosphere."*

This is the destination-first plan for that mission — grounded in a read-only recon
of the whole repo (8 axes, 2026-07-22), tier-honest throughout. It names the
pinnacle end-state first (Operating Principle #0), then where sui stands today, the
concrete gap, the highest-leverage first moves, and the phased path. Companion docs:
[`SPEED.md`](./SPEED.md) (the perf strategy + lever ledger) and
[`SUI-EQUIVALENCE.md`](./SUI-EQUIVALENCE.md) (the equivalence surfaces).

---

## 1. DESTINATION (the pinnacle, unhedged)

**sui is the drop-in Nix.** Every invocation the Nix ecosystem can make — every
language construct, every builtin, every CLI verb, every fetcher, every derivation,
every whole-system closure — is served by sui, and:

- **Byte-parity-correct by proof, not by sample.** For any pinned toplevel (up to and
  including the full `cid` system closure, ~20,827 drvs), *one* sui instantiation and
  *one* cppnix instantiation produce byte-identical ATerm derivations, byte-identical
  output paths, and byte-identical realized NARs at **every node in the graph** —
  verified continuously in CI against a pinned cppnix oracle, any divergence
  auto-localized to a single drv. "Every use case" is not 77 hand-authored
  expressions; it is *every drv in the real closure* plus the entire CppNix functional
  `lang/` corpus (eval-okay/eval-fail/parse-okay/parse-fail) plus all seven
  equivalence surfaces S1–S7 (eval / instantiation / realization / CLI-contract /
  config / daemon-protocol / drop-in-PATH).
- **Measurably faster than cppnix across the entire use-case matrix U01–U12** — not
  just the shallow wins already banked (tiny-expr, single-file hot shapes,
  `hello.drvPath`), but the two currently-losing cells: **U05 deep recursion** (today
  9.4× slower) at parity-or-better, and **U10 whole-system eval** (today cannot
  complete — dies by swap at ~73 GB) completing under an 8 GB ceiling *faster* than
  cppnix's 121 s. The eval cache's measured 407× repeat-eval win becomes the
  steady-state operator experience.
- **Beautiful and clear by construction.** The 37-domain TYPED-SPEC triplet discipline
  reaches the engine core: builtins live *once* (not triplicated across tree-walker /
  VM / IR value types); a single `[workspace.lints]` pedantic-plus-deny posture covers
  all ~160k lines; the byte-critical ATerm serializer emits through a typed AST, not
  `format!()`. "The engines cannot diverge" is true *at the builtin surface*, not only
  in the 37 extracted domains.
- **Runtime-fluid in a way cppnix structurally cannot be.** Eval strategy — eager/lazy
  class, attrset shape, thunk-retention policy — is a **live, parity-typed
  conversation**: `(defeager-class …)` / `(defshape …)` / `(defretention-policy …)`
  tatara-lisp rules hot-reload through sui-daemon's shikumi `ConfigStore`, each refused
  at the patch border unless its `perf.rs::earned_tier` is `ByteSufficient`. Steering
  changes *how fast*, structurally never *the bytes*. The optimizer is a moving
  setpoint the agent tunes per use-case-class (afinar), not a set of compiled-in
  constants.

This is the absolute-best long-term answer: a Nix that is provably equivalent,
uniformly faster, self-describing, and steerable — owned end-to-end in the pleme-io
substrate.

---

## 2. WHERE SUI STANDS TODAY (tier-honest, per axis)

| Axis | Coverage / posture | Biggest single gap |
|---|---|---|
| **Eval-surface** | **~95% of the language, SHIPPED** in the default tree-walker (`sui-eval`, `TreeWalkEvaluator`): full rnix AST, full operator set, cppnix lazy/Blackhole/Promise model, tracked string-context, ~110 builtins, input-addressed + FOD `derivationStrict` store-path hashing, gix fetchers, native flake.lock. VM (`sui-bytecode`) is a **fallback accelerator, not a second oracle**. | `overlay-fixpoint` mis-classification (`laziness.lisp`) silently forks drv hashes on real nixpkgs overlays; plus `pipe-ops`, `__curPos`, missing `fetchClosure`/`outputOf`. |
| **CLI** | 109-entry catalog; routing bridge (`sui-nix-wrap`, longest-prefix, no cppnix fallback, exit-78 on gap) SHIPPED + tested. Headline "77 Working" is **self-asserted, not implementation-gated**: ≥12 rows are stub/no-op/shell-out → **genuinely-working ≈ 65**. | `repl/why/edit/log/fmt/bundle/print-dev-env/upgrade-nix` catalogued `Working` but return `NotImplemented`; `collect-garbage` no-op; `search` shells to real `nix`. |
| **Build / derivation / store** | Local builder + daemon-realize worker-protocol client + signature+NAR-hash-verifying binary-cache substituter + mark-and-sweep GC all **SHIPPED and real**. drvPath byte-exact for **leaf / 2-level / multi-output / FOD** fixtures. | Deep-fixpoint **drops string-context dependency EDGES** (`openssl`→coreutils, `xgcc`→zlib.out); fetchers write to `/tmp` with **placeholder narHashes** (`fetchGit` = `sha256(rev)`). |
| **Perf** | Parity **77/77 green** (precondition). U01 win; **U02 1.86× engine geomean**; **U04 `hello.drvPath` ~1.4× faster, byte-identical**; **eval cache 407×**; 3 Proven CPU levers. | **U05 deep recursion 9.4× SLOWER**; **U10 cid toplevel = G0 RED — cannot complete** (swap-death). The entire L0–L15 memory campaign is **DESIGN, zero code**. |
| **Test / parity infra** | `sui parity` (77 rows, CONVERGE=SEAL, CI-gated `parity.yml`) + `perf-seal` + 3-slice sui-ir differential + ~180 oracle cases + 25 lang fixtures + `build-parity`. | Corpus is **regression-derived, not systematic**; **non-hermetic** (skips pass green offline); only **S1 of 7** equivalence surfaces; full cppnix `lang/` corpus **not vendored**; no differential fuzzer. |
| **IR / L3 frontier** | `eval_ir` reaches a **three-way byte match (eval_ir == tree-walker == nix) on real nixpkgs `bootstrap-stage-xgcc-stdenv` drvPath**; total lowering (23/24 variants), ~93 native builtins, rec-semantics promotion. | **Production surface = 0%** (nothing depends on sui-ir); `hello.drvPath` print-only, diverges; ProgramCache **path-keyed** (must be BLAKE3 content-hash before `--ir` flip). |
| **tatara-lisp fluidity** | **~95% DESIGN.** SHIPPED: the perf-lever Technique taxonomy (`perf.rs`/`perf.lisp` — the law's gate), spec-triplet machinery, laziness *classifier*, generic shikumi hot-reload (daemon transport only). | `(defeager-class)`/`(defshape)`/`(defretention-policy)` **do not exist**; no afinar wiring; the parity-typed-knob law is **prose in SPEED.md §VIII** with no enforcement border. |
| **Clarity / beauty** | 37-domain TYPED-SPEC triplet **shipped + both engines wired to one interpreter**; `Lazy<T>` + compile-time `Value` size seal; clean naming + member docs. | **Builtins TRIPLICATED** across `sui-eval` / `sui-bytecode` / `sui-ir` (~4k lines of hand-mirror); **no `[workspace.lints]`** → the biggest crate (`sui-eval`, 34.5k LoC) is **unlinted**; ATerm serializer uses `format!()`. |

**The single fact that binds three axes:** the **"materialized-attrset-with-keys
partial"** defect (`sui-eval/src/builtins/derivation.rs` §NOTE 2026-07-10, mirrored in
`sui-ir/src/derivation.rs:43-63`, rooted in the fixpoint promotion at
`sui-eval/src/value.rs` ~1606) is *simultaneously* the build-derivation biggest
opportunity, the ir-l3 next gap, and the eval-surface `overlay-fixpoint` root. **One
fix moves all three.**

---

## 3. THE GAP — what "every single possible nix use case" concretely means

"Every use case" is a **product** of five independent test surfaces; the current
corpus covers a thin slice of one:

1. **Language surface (S1).** Full grammar × complete operator table × all ~120
   builtins × error paths — *systematic enumeration*. Today: 77 **regression-derived**
   rows; the full CppNix `tests/functional/lang/` corpus is **not vendored** (only 25
   hand-picked fixtures).
2. **Whole-closure surface (S2/S3).** Per-node ATerm byte-diff (instantiation) +
   per-node NAR byte-diff (realization) across the *entire* dep graph. Today:
   `bisect_drv` descends only the **first** diverging child; the one eval that turns 77
   rows into a 20,827-node theorem (`cid` toplevel, U10) **cannot complete**.
3. **CLI-contract surface (S4).** Every flag/JSON-shape/exit-code matches. Today:
   name-level only; `sui build` *discards every flag*.
4. **Config + daemon + drop-in (S5/S6/S7).** `nix.conf`/`NIX_PATH` parsing (absent);
   the daemon **server** side (client-only today); PATH drop-in (blocked).
5. **Performance surface (U01–U12).** Every class at gate G2. Today: ~4 cells green,
   U05 loses 9.4×, U10 cannot run.

**Distance:** the shipped seal proves *"zero unexpected regressions among ≤77 rows that
ran on a host with nixpkgs present"* — real and valuable, but ~**1%** of the
destination's surface. The nearest tractable proxy for "exhaustive": **make one
`cid`-closure instantiation byte-diff every node** — converting a 77-expression claim
into a ~20,827-node theorem from two evaluations. Gated on U10 completing.

---

## 4. HIGHEST-LEVERAGE FIRST MOVES (ordered by compounding)

**M1 (keystone) — land the "materialized-attrset-with-keys partial" fix.** Make the
promoted fixpoint partial carry its materialized keys (`sui-eval/src/value.rs` ~1606
promotion + `sui-eval/src/builtins/derivation.rs` §NOTE + mirror in
`sui-ir/src/derivation.rs`), so string-coercion in a dep position succeeds and the
dependency **edge is retained** through the stdenv/package fixpoint. *One edge* between
shallow byte-parity and real-nixpkgs-leaf byte-parity; fixes the tree-walker **and**
`eval_ir` at once; flips `nixpkgs_hello_frontier` print→assert; precondition for M2 +
M5. **High effort, byte-critical — deserves a focused session; guard with the full
40-min parity corpus, not just the fast differential.**

**M2 — whole-closure graph-walk differential (S2/S3).** Generalize `bisect_drv`
(`src/main.rs:4763`) from first-child-descent into a **visit-all** walk against a
pinned/vendored cppnix; per-node ATerm then NAR byte-diff; CI gate. Converts the seal
into a whole-closure theorem with auto-localization. Gated on M1 + M4.

**M3 — vendor the full CppNix `lang/` corpus + CLI honesty gate.** (a) Vendor
`tests/functional/lang/`. (b) Extend `sui-spec/tests/cli_coverage_invariants.rs` so a
`Working` row **fails the build** if its handler is an unconditional `NotImplemented` /
no-op / shell-out — truthful coverage, mechanically. Low–medium.

**M4 — unblock U10: fresh dhat (L0) → columnar+shape attrsets (L6) + trimmed thunk
capture (L7).** Bend the ~140 MB/s retained-allocation slope below the completion line.
U10 is the only cell where nix wins by completing at all, and the load-bearing
dependency for M2's proof. High; L0 cheap and mandatory-first.

**M5 — flip `--ir` to the default engine behind the byte-diff.** After M1 (hello
matches) and after migrating `PROGRAM_CACHE` from path-keying to **BLAKE3 content-hash
keying** (makes the `(source_id,offset)` cache-unsoundness class unrepresentable), make
`eval_ir` the sacred path. L3 attacks the largest measured cost (rowan re-walk 40.7%
heap / 21% wall), is micro-proven 2.5–3.4× warm, and is the only lever that can move
*both* U05 and U10.

**M6 — collapse triplicated builtins + hoist `[workspace.lints]`.** (a) One
`[workspace.lints]` (pedantic + curated deny), closing ~80k unlinted lines incl.
`sui-eval`. (b) Collapse the three hand-mirrored builtin impls into a spec-driven core.
(c) Route the ATerm serializer through a typed AST (kill the byte-path `format!()`).
Sequence the lint hoist first (cheap).

**M7 — ship the FIRST parity-typed knob.** Expose one already-`ByteSufficient` lever as
a `(defeager-class …)` rule through sui-daemon's shikumi `ConfigStore` hot-reload, with
the knob-application border calling `perf.rs::earned_tier` to **REFUSE** any non-`ByteSufficient`
rule. Converts the parity-typed-knob law from prose to one enforced border — *wiring,
not invention*. The tatara-lisp fluidity beachhead.

---

## 5. THE PHASED PATH (M0 → M7, each shippable + verifiable)

- **M0 — Honesty baseline (ship now, days).** CLI honesty gate (M3b) +
  `[workspace.lints]` (M6a) + record M4's fresh dhat (L0). *No behavior change* — pure
  truth-in-labeling + measurement. Verifiable: parity still green; "Working" count
  drops to the truthful ~65; uniform clippy posture; a committed dhat profile exists.
- **M1 — Deep-fixpoint edge preservation (weeks).** The materialized-attrset partial.
  Verifiable: `nixpkgs_hello_frontier` flips print→assert; `overlay-fixpoint` rows
  graduate; `openssl`/`xgcc`/`libxcrypt` edges retained; **full corpus green**.
- **M2 — Hermetic CI parity + whole-closure differential (weeks).** Pin a cppnix binary
  in CI (zero skips); vendor the full `lang/` corpus; visit-all ATerm/NAR graph walk.
- **M3 — U10 memory wall down (months).** L6 columnar/shape attrsets + L7 trimmed
  capture. `cid` toplevel completes under 8 GB; the whole-closure differential runs on
  the full 20,827-node closure — "every use case ≈ every drv."
- **M4 — `--ir` becomes the engine (months).** BLAKE3 content-hash ProgramCache; flip
  `--ir` default behind the byte-diff. U05 at parity-or-better; the `(source_id,offset)`
  unsoundness class gone.
- **M5 — Builtin unification + typed emission (months).** One builtin core; ATerm via
  typed AST; zero `format!()` in the derivation serializer.
- **M6 — Runtime fluidity live (parallel from M0).** M7 first knob → the full
  `(defeager-class)`/`(defshape)`/`(defretention-policy)` vocabulary through
  sui-daemon shikumi hot-reload, each gated by `earned_tier`; afinar MCP panel.
- **M7 — Drop-in completeness (long tail: S4–S7).** Real CLI flag/JSON/exit contract;
  `nix.conf`/`NIX_PATH` (S5); the daemon **server** side (S6); PATH drop-in (S7);
  `pipe-ops`/`__curPos`/`fetchClosure`/`outputOf`; store-honest fetchers.

**Destination reached when:** (a) the whole-closure differential is green on the full
`cid` closure on the IR engine (M3+M4), (b) all seven equivalence surfaces are CI-gated
against a pinned cppnix (M2+M7), (c) U01–U12 all sit at G2 (M3+M4), and (d) eval
strategy is a live parity-typed conversation (M6). Each phase ships a real thing proven
against reality; none waits on the phase after it.

---

**One-line synthesis:** sui today is a *real, deep, default-engine Nix that is
shallow-parity-proven and locally faster* — the tree-walker genuinely evaluates the
language, realizes shallow closures, and beats cppnix on the cells it can run. The
three things between that and the destination are all named and localized: the
deep-fixpoint edge-preservation fix (one change, three axes), the U10 memory wall, and
the promotion of the seal from 77 regression rows to a whole-closure theorem. The
tatara-lisp fluidity that shoots perf "into the stratosphere" is ~95% design — but its
gate-substrate (the perf-ledger `Technique` taxonomy + shikumi hot-reload) already
ships, so the first enforced knob is *wiring, not invention*.
