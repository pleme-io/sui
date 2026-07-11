# Sui Macro Vocabulary — the generate-don't-author destination

> **★★ EMITTER SUBSTRATE + Pillar 12 (generation over composition), applied to
> the sui landscape.** This is the destination-first plan (Operating Principle
> #0) for maximizing *generated* code across sui: every recurring impl-shape and
> every typed problem-space table becomes an authored declaration a macro
> expands, not a hand-kept match. Modeled on the org's canonical worked
> reference — [`mado/docs/MACRO-VOCABULARY.md`](https://github.com/pleme-io/mado/blob/main/docs/MACRO-VOCABULARY.md).
> Authored 2026-07-11 (RECON + DESIGN phase — READ-ONLY on all `src/`).

## The destination (named first)

**sui = one typed byte/wire algebra (opcodes + worker-protocol ops + hash/CA
kind tables) + operator-authored problem-space specs (sui-spec's 35 TYPED-SPEC
triplets), sitting on the tatara-rust-ast macro farm, with zero hand-written
byte↔variant / string↔variant round-trip matches.** Two layers, and — per the
mado learning — **only two things pay**:

- **Layer B — problem-space wire/byte tables (the real leverage).** One
  algorithm (a byte↔variant round-trip, a wire-op decode, an operand-arity
  classification) transcribed N times against a table of cases. One authored
  table + a generated interpreter kills a whole **drift class**. sui already
  proves the mechanism 35 times inside `sui-spec` (the TYPED-SPEC triplet: Rust
  border + `(def…)` Lisp + `apply` behind a mockable Environment). The prize is
  the runtime crates (`sui-bytecode`, `sui-compat`) that hand-keep byte tables
  the spec crate *also* declares — a **cross-crate drift** the substrate has not
  yet closed.

- **Layer A — genuine impl-shape duplication (narrow).** A *real* hand-kept
  `Display`/`FromStr`/`as_str`/`from_byte` match already exists on an enum. An
  existing farm derive (`KindStr` / `KindByte`) collapses it — **only where the
  hand table is real**. The sweep below found this is **rare** in sui: only 5
  files carry a real hand-written string/byte table.

**What does NOT pay (LEAVE-ALONE — flagged loudly in the rejection list):**
`thiserror` error enums (thiserror IS the canonical macro), `#[serde(rename_all)]`
enums (serde owns the string table — adding `KindStr` = speculative unused API),
SeaORM `Relation` enums (framework-derived), plain payload-carrying enums with
no round-trip, and one-off enums below the recurrence bar. Force-fitting a derive
onto any of these is over-abstraction — debt on the same "duplication is a bug"
budget.

The test for every candidate: **does the THIRD use demonstrably reuse the SAME
shape?** If yes → macro. If no → inline, leave alone.

## The macro farm — what sui consumes / can consume

`tatara-rust-ast` (`catalogs/pleme-derives.lisp`) publishes **30 derives**. The
ones this plan touches — **all already published; NO new farm derive is required
to start execution**:

| Derive | Generates | Requires | Attrs |
|---|---|---|---|
| `KindStr` | `as_str(&self) -> &'static str` + `from_str_kind(&str) -> Option<Self>`, folded from one variant walk so the inverse-table property holds by construction | unit variants | `#[kind(name=…, alias=…)]` (default name = ident) |
| `KindByte` | the `KindStr` pair **plus** `as_byte(&self) -> u8` + `from_byte(u8) -> Option<Self>` | unit variants; `#[kind(byte=N)]` on **every** variant | `#[kind(name=…, alias=…, byte=N)]` |
| `AllVariants` | `pub const ALL: &[Self]` + `pub const fn all()` | unit variants | — |
| `VariantStr` / `VariantNames` / `VariantCount` | `as_str` name fold / `ALL_NAMES` / count | unit variants | — |
| `Display` | `impl Display` folding `as_str` | has `as_str` | — |
| `ImplFrom` / `AsRef` / `Deref` / `Inner` / `FromStrNewtype` | newtype conversions | single-field newtype | — |

A **new** derive lands as one `catalogs/pleme-derives.lisp` entry →
`tatara-rust-forge catalog-instantiate` emits + verifies + publishes the crate
(★★ EMITTER SUBSTRATE: author the Spec, not the proc-macro; publish upstream,
*then* consume — never inline a derive in sui). **This plan needs none** — the
Layer B tables want a **local `macro_rules!` / `(defworker-opcode)`-style spec**
(the byte/operand values are load-bearing and enum-specific, not a generic
impl shape), and the Layer A candidates all fit `KindStr`/`KindByte` as-shipped.

---

## Per-crate recurring-shape catalog

Site counts are **verified from source** (2026-07-11). "sites" = the number of
hand-written copies of the shape a single authored declaration collapses.

### sui-bytecode (13,045 LOC) — the VM / opcode crate — **the Layer B prize**

| Shape | Where | Layer | Mechanism | Sites | Recurrence justification |
|---|---|---|---|---|---|
| **`OpCode` byte↔variant round-trip** | `opcode.rs` — `#[repr(u8)]` enum (57 variants w/ explicit bytes) **+** hand-written 57-arm `from_byte` match **+** hand-written 57-element `roundtrip_all_opcodes` test array | **B** | local `macro_rules! opcodes! { Name = N, … }` OR `(defopcode)` spec — one table → the `#[repr(u8)]` enum, `from_byte`, `as u8`, `ALL`, and the roundtrip test | **3** (enum decl · `from_byte` · test array) — a 4th if the compiler's emit sites are folded | A new opcode today must be added in **3 places** by hand; the test array *silently passes* if you forget a variant (it only checks what's listed). This is the exact drift shape mado's M4 found a live bug in. |
| **`OpCode` operand-arity classification** | `chunk.rs` disassembler match (0/1/2 u16 operands) — the same arity is implicit in `compiler.rs` emit + `vm.rs` decode | **B** | fold operand-arity into the same `opcodes!` table (`#[operands=1]`) → generate the disassembler arity match | **1 explicit** (+ 2 implicit consistency obligations in compiler/vm) | Operand count is a per-opcode property transcribed as a classification match; one authored column removes the "add opcode, forget its arity in the disassembler" drift. |
| `CompileError`/`VMError`/`EvalError` enums | `error.rs`, `lib.rs` — `thiserror` | — | **LEAVE-ALONE** (thiserror) | — | thiserror IS the macro for this shape. |
| `HeapObject`/`VMValue`/`ThunkState`/`HigherOrderOp` | `nanbox.rs`, `value.rs` | — | **LEAVE-ALONE** | — | Payload-carrying value enums; no round-trip table; ergonomic hand-matched dispatch. |

### sui-compat (8,925 LOC) — the wire / hash / store-path parse⇆emit crate

| Shape | Where | Layer | Mechanism | Sites | Recurrence justification |
|---|---|---|---|---|---|
| **`WorkerOp` wire-op round-trip** | `wire.rs` — `#[repr(u64)]` enum (**42 variants**) **+** hand-written 42-arm `TryFrom<u64>` match | **B** | `KindByte`-shaped BUT u64 → a local `worker_ops! { Name = N, … }` macro (byte→u64), OR **consume the sui-spec `(defworker-opcode)` catalog** (see cross-crate note) | **2** (enum decl · `TryFrom` match) | Classic wire-decode double-transcription. |
| **`WorkerOp` ⟷ sui-spec `worker_protocol` DRIFT** | runtime `wire.rs` (42 ops) vs `sui-spec/specs/worker_protocol.lisp` (**33** `defworker-opcode`) | **B** | make the spec the single source; runtime consumes / is byte-pinned against it | cross-crate | **LIVE DRIFT, verified: 42 ≠ 33.** Two authoritative op tables exist for the *same* protocol and have already diverged by 9 ops. The flagship finding — parallel to mado's mode-12 drift. |
| **`HashAlgorithm` string round-trip** | `hash.rs` — Display match (4) + `FromStr` match (4) + `as_nix_str` (4) | **A** | `#[derive(KindStr)]` with `#[kind(name="sha256")]` (names ≠ idents, lowercased) | **3** (Display · FromStr · as_nix_str) | Real hand table, 3 copies of the same 4-entry map; textbook `KindStr`. `digest_len` stays hand-written (a computed property, not a name table). |
| **`ContentAddressMethod` string round-trip** | `content_address.rs` — Display match (3) + `FromStr` match (3) | **A** | `#[derive(KindStr)]` (`text`/`flat`/`recursive` — lowercase ident names) | **2** | Real hand table; clean `KindStr` fit. |
| `StderrMsg` magic-constant enum | `wire.rs` — `#[repr(u64)]`, values are ASCII magic (`"olmg"` etc.) | **B (small)** | `worker_ops!`-style table, low priority | 1 | Only a decl today (no decode match yet); below the bar until a decoder is written. |
| `WireError`/`HashError`/`NarError`/`StorePathError`/… (10 error enums, 31 arms) | across the crate | — | **LEAVE-ALONE** (thiserror + `#[from]`) | — | thiserror + `#[from]` cascade is the canonical shape. |
| `StorePath`/`ContentAddress`/`NarNode` FromStr parsers | `store_path.rs`, `nar.rs` | — | **LEAVE-ALONE** | — | Real hand-written *grammar* parsers (not name tables); already specced in sui-spec (`nar`, `store_layout`); no mechanical table to fold. |

### sui-orchestrate (5,223 LOC)

| Shape | Where | Layer | Mechanism | Sites | Recurrence justification |
|---|---|---|---|---|---|
| `Platform` string round-trip | `system.rs` — Display (2) + `FromStr` (2) | **A (small)** | `#[derive(KindStr)]` `#[kind(name="darwin"/"nixos")]` | **2** | Real hand table but only 2 variants; a *borderline* KindStr fit — collapse only when touched (opportunistic, not a milestone of its own). `rebuild_command`/`detect` stay hand-written. |
| `RebuildAction`/`NodeStatus`/`DeployStrategy`/`DeployOrder` | `system.rs`, `node.rs`, `fleet.rs` | — | **LEAVE-ALONE** | — | Plain enums, no hand string table (dispatched by match on the variant, not a name map). |
| `FleetError`/`CommandError`/… (17 arms) | across | — | **LEAVE-ALONE** (thiserror) | — | — |

### sui-supercacheci (5,649 LOC)

| Shape | Where | Layer | Mechanism | Sites | Recurrence justification |
|---|---|---|---|---|---|
| `TrackedInputKind` Display | `preheat.rs` — Display match (10 arms), non-ident names (`flake.lock`, `Cargo.lock`, `go.mod`…) | **A (Display-only)** | `#[derive(VariantStr)]`/`Display` derive w/ `#[kind(name=…)]` — but **no parse direction** | **1** | One-direction table, single site; a `KindStr` paired-inverse would add an unused `from_str_kind`. Marginal — collapse only if a parse direction is ever added. |
| `StoreBackendKind`/`CacheBackendKind`/`ObjectStoreKind`/`GenDomain`/`Arch`/`ControllerTier`/`CacheTier`/`TuneKnob`/… (20+ enums) | `lib.rs`, `memory.rs`, `controller.rs` | — | **LEAVE-ALONE** | — | **All `#[serde(rename_all="snake_case")]`** — serde owns the string table. No hand-written match to collapse. Adding `KindStr` = speculative unused API (the mado over-abstraction rejection, verbatim). |
| shikumi config structs (`SuperCacheCiConfig` + tiers) | `lib.rs` | — | **LEAVE-ALONE** | — | shikumi `TieredConfig` is the canonical config mechanism; already adopted. |

### sui-bigorna (2,211 LOC) — docker/buildx build front

| Shape | Where | Layer | Mechanism | Sites | Recurrence justification |
|---|---|---|---|---|---|
| `Os`/`Arch`/`BuildxDriver`/`CacheMode`/`CacheDirection`/`EmulationVerdict`/`BuildOutcome`/… | `platform.rs`, `node.rs`, `cache_front.rs`, `builder.rs`, `lib.rs` | — | **LEAVE-ALONE** (verify per-enum on touch) | — | Sampled: no hand-written string round-trip tables; dispatch is by-variant match. If any grows a Display+FromStr pair later, revisit as a `KindStr` site. |

### sui-store (9,469 LOC) / sui-cache (4,165 LOC) / sui-daemon (4,801 LOC) / sui-graph-store (1,167 LOC) / sui-build (6,005 LOC) / sui-protocol / sui-daemon-frame / sui-daemon-client / sui-intern / sui-nix-wrap / sui-cache-eval

| Shape | Where | Layer | Mechanism | Notes |
|---|---|---|---|---|
| `StoreBackend`/`SubstituteResult`/`Relation`/mode enums | sui-store | — | **LEAVE-ALONE** | `Relation` = SeaORM-derived; mode enums plain; store-op algorithms already specced in sui-spec (`store_ops`, `store_query`, `store_transform`, `store_recipe`, `store_inventory`, `store_layout` — 6 triplets). |
| `layout.rs` as_str table (5 arms) | sui-graph-store | **A (small)** | `VariantStr`/`Display` derive | Single as_str-only site; marginal — collapse only on touch. |
| error enums (5 / 20 / 8 / 10 / 18 arms) | store/cache/daemon/graph-store/build | — | **LEAVE-ALONE** (thiserror) | thiserror is the macro. |
| everything else | — | — | **LEAVE-ALONE** | Pure glue / value structs / no recurring mechanical table. |

### sui-spec (26,066 LOC) — **already the destination; do not re-abstract**

sui-spec is the reference implementation of the TYPED-SPEC + INTERPRETER triplet
and CATALOG REFLECTION: **35 `(def…)` domains**, each a `#[derive(DeriveTataraDomain)]`
Rust border + a `specs/<domain>.lisp` + an `apply` interpreter, self-described
by `catalog.rs`/`catalog.lisp`. This crate is **already generation-native** and
is the *model* the runtime crates should reach toward — it is **not a refactor
target**. Its only role in this plan: it is the **single source of truth** the
runtime tables (`WorkerOp`, and any future opcode spec) should be pinned against
(cross-crate section below).

---

## Cross-crate opportunity — the one that matters

**`WorkerOp` (sui-compat runtime, 42 hand-arms) ⟷ `worker_protocol` (sui-spec, 33
`defworker-opcode`).** Two authoritative tables for the *same* Nix daemon wire
protocol, **verified drifted (42 ≠ 33)**. This is the single highest-leverage
kill in the landscape because it (a) is a live drift, (b) spans two crates, and
(c) the *spec side already exists* — the work is mostly *pinning*, not authoring.
The destination: sui-spec's `(defworker-opcode)` catalog is the source; sui-compat's
`WorkerOp::TryFrom<u64>` is either generated from it or byte-pinned against it by a
cross-crate test that fails the build when the two op sets differ. (Sequence this
*with the eval wave* — see phasing — because the worker protocol is touched by
daemon/eval paths.)

---

## ★ The core learning (carried from mado, re-confirmed against sui)

"Maximize macros" ≠ "derive everything." Macro leverage splits cleanly in two,
and **only one half pays**, and the split is *sharper* in sui than in mado:

1. **Domain / problem-space byte+wire tables (Layer B) — the real prize.** The
   `OpCode` byte round-trip + operand-arity, the `WorkerOp` wire round-trip, the
   `WorkerOp`⟷spec drift. One authored table + a generated decoder eliminates a
   whole drift class. Mechanism: a **local `macro_rules!`** (byte values are
   load-bearing and enum-specific — NOT a generic impl shape a farm derive
   should own) or a **`(defopcode)`/`(defworker-opcode)` TataraDomain spec** with
   an `apply` decoder.

2. **Genuine string/byte impl-shape duplication (Layer A, narrow).** Only where a
   *real* hand-written `Display`+`FromStr`+`as_str` match exists — in sui that is
   **exactly 3 enums worth doing** (`HashAlgorithm`, `ContentAddressMethod`, and
   opportunistically `Platform`), collapsible with the *already-published*
   `KindStr` derive.

**What does NOT pay — and adopting it is over-abstraction (forbidden):** the 20+
`#[serde(rename_all)]` enums in sui-supercacheci/sui-store, the thiserror error
enums fleet-wide, the SeaORM `Relation` enums, the plain by-variant-dispatch
enums. None has a hand-written table to collapse; deriving `KindStr` onto them
*adds* unused `as_str`/`from_str_kind` API. **A mature crate's remaining hand
methods are hand-written because the generic shape doesn't fit.**

The rule this yields: **generate the mechanical byte/wire/name table once; keep
the ergonomic dispatch hand-crafted.** Never derive for the sake of a derive count.

---

## Committed REJECTION LIST (over-abstraction is debt too)

Do **not** re-propose these — each was evaluated against source and rejected:

- **`KindStr`/`KindByte` on every `*Kind` enum in sui-supercacheci** (`StoreBackendKind`,
  `CacheBackendKind`, `ObjectStoreKind`, `GenDomain`, `Arch`, `ControllerTier`,
  `CacheTier`, `TuneKnob`, `PreheatTier`, `TrackedInputKind`, …) — **all carry
  `#[serde(rename_all="snake_case")]`**; serde already owns their string table.
  There is **no hand-written match to collapse.** Deriving `KindStr` adds an
  unused `as_str`/`from_str_kind` surface. Verified non-fit — the mado
  over-abstraction rejection, verbatim.
- **A farm derive for the error enums** (sui-compat 31 arms, sui-store 20,
  sui-bytecode 22, sui-build 18, sui-orchestrate 17, …). `thiserror` + `#[from]`
  IS the canonical macro for this shape. A new derive would fork error handling
  for zero gain.
- **`KindStr` on `Os`/`Arch`/`BuildxDriver`/`CacheMode`/`RebuildAction`/`NodeStatus`/
  `DeployStrategy`** and the other plain enums — no hand-written string round-trip;
  they are dispatched by *matching the variant*, not by a name map. Speculative API.
- **`GetterAll`/`WithBuilder`/`SetterAll` on the config/value structs** (e.g.
  `SuperCacheCiConfig`, `Endpoint`, `NixHash`, `ContentAddress`) — the farm's
  generic per-field derives emit *by-reference, direct-assign, all-fields* methods;
  sui's constructors are ergonomic (`NixHash::new(algo, digest)`, `Endpoint::unset()`)
  and its config is shikumi-`TieredConfig`-owned. Force-fitting regresses ergonomics
  and exposes internals. (Same verdict mado reached.)
- **A new derive for the store-op / NAR / narinfo grammars** — these are real
  hand-written *parsers* (grammar, not name tables) and are *already* declared as
  sui-spec triplets. Nothing mechanical to fold.
- **Touching sui-spec's 35 domains.** It is already the generation-native model.
  Re-abstracting it is churn.
- **`StderrMsg` decode table** and the small single-site `as_str`-only tables
  (`preheat.rs` `TrackedInputKind` Display, `graph-store/layout.rs`) — below the
  recurrence bar today (one direction, one site). Note them; collapse *only* if a
  second direction/site appears (the 3-use rule).

---

## Top-5 highest-leverage macro families to build first

Ordered by drift-class kill. **Every one is reuse or a local macro — NO new farm
derive must be published before execution starts.**

| # | Family | Layer | Kind | Collapses | Why first |
|---|---|---|---|---|---|
| **1** | **`WorkerOp` ⟷ sui-spec pin** | B | **local test + (existing) `(defworker-opcode)` spec** | the live 42≠33 cross-crate drift | Highest leverage: a *live* drift across two crates; the spec side already exists — mostly pinning. |
| **2** | **`opcodes!` table** (`OpCode` byte round-trip + operand arity) | B | **local `macro_rules!`** (or `(defopcode)` spec) | 3 hand copies (enum · `from_byte` · roundtrip test) + the disassembler arity match | The VM's core drift shape; directly parallels mado M4. Adds an *exhaustive* roundtrip forcing-function. |
| **3** | **`HashAlgorithm` `KindStr`** | A | **reuse `pleme-kindstr-derive`** | 3 hand copies (Display · FromStr · as_nix_str) | Cheapest real Layer-A win; farm derive shipped. |
| **4** | **`ContentAddressMethod` `KindStr`** | A | **reuse `pleme-kindstr-derive`** | 2 hand copies | Same shape as #3; batch them into one milestone. |
| **5** | **`worker_ops!` runtime table** (or generate from #1's spec) | B | **local `macro_rules!`** | 2 hand copies (enum decl · `TryFrom<u64>`) | Closes the runtime side of #1 so the two tables can never drift again. |

Below the top-5, **opportunistic** (do only when the file is touched, never as a
milestone): `Platform` `KindStr` (2 sites), the two single-site `Display`-only
tables. **Everything else: LEAVE-ALONE.**

---

## Phased execution plan (sui-eval LAST — hard dependency)

> **★ HARD SEQUENCING DEPENDENCY.** `sui-eval` (29,327 LOC — the largest crate)
> is under **concurrent byte-parity editing** by another agent. **No phase below
> touches `sui-eval/src` until the byte-parity wave has landed and merged.**
> sui-eval's own `register_numeric_binop!` / `register_bitwise!` /
> `register_string_predicate!` local macros already prove the builtin-registration
> table shape recurs (3 sites) — a `register_builtins!` consolidation is the
> natural sui-eval milestone, but it is **explicitly deferred to the final phase**.

| P | Scope | What | Risk | Depends on |
|---|---|---|---|---|
| **P0** | sui-compat | **`HashAlgorithm` + `ContentAddressMethod` → `KindStr`** (reuse). Byte-pin: assert `as_str`/`from_str` byte-identical to the current match arms before deleting them. | low | farm `KindStr` (shipped) |
| **P1** | sui-bytecode | **`opcodes!` local `macro_rules!`** — one table generates the `#[repr(u8)]` enum, `from_byte`, `as u8`, `ALL`, **and an exhaustive roundtrip test** (kills the "forgot a variant in the test array" hole). Byte-pin every arm to the current literal. | med | none |
| **P2** | sui-bytecode | Fold **operand arity** into the P1 `opcodes!` table (`#[operands=N]`); generate the disassembler arity match in `chunk.rs`. Byte-pin disassembly output. | med | P1 |
| **P3** | sui-spec + sui-compat | **`WorkerOp` ⟷ `(defworker-opcode)` pin.** Add a cross-crate test asserting the runtime op set == the spec op set; reconcile the 42/33 drift (decide the true op list *with the daemon/eval owners*). Then generate/pin `WorkerOp::TryFrom<u64>` from the spec (a `worker_ops!` local macro is the interim if generation-from-spec is too heavy). | med–high | touches the daemon wire path — **coordinate with, ideally follow, the eval wave** |
| **P4** | opportunistic | `Platform` `KindStr`, single-site `Display` tables — **only when those files are touched for other reasons.** | low | — |
| **P5** | **sui-eval (LAST)** | After the byte-parity wave merges: **`register_builtins!`** consolidating the 3 existing local registration macros into one authored builtin-registration table (per-builtin `#[arity]`/`#[curried]` column). | med | **byte-parity wave merged** |

Each phase is independently shippable and **byte-pin-gated**: diff the old
match/literal into the assertion *before* refactoring, then prove the generated
output is byte-identical. `cargo build && cargo test` green (via `nix develop`
where the crate needs it) is the close-gate for every phase.

---

## Test strategy (sui's own idioms, carried forward)

1. **Byte-pin tests** — every codegen'd surface (opcode `from_byte`, `WorkerOp`
   `TryFrom`, `KindStr` `as_str`/`from_str`) asserted **byte-identical to the
   pre-refactor literal**, not merely self-consistent. sui-bytecode's existing
   `roundtrip_all_opcodes` is the model — but the P1 table makes it
   **exhaustive** (a new variant with no test entry becomes impossible, closing
   the current silent-pass hole).
2. **Cross-crate drift forcing-function** — P3 ships a test in sui-spec (or a
   shared test crate) that fails the build when `WorkerOp`'s variant set and the
   `(defworker-opcode)` catalog's op set differ. This is the mechanical promise
   that the two tables can never drift again (the whole point of the phase).
3. **CATALOG REFLECTION** — any *new* `(def…)` domain (if P3 elects a full
   `(defopcode)`/`(defworker-opcode)` extension over a local macro) lands its
   `catalog.lisp` entry **in the same commit**, per sui-spec's existing
   substrate-invariant tests (every domain has a catalog entry; every keyword is
   unique; the dependency DAG is acyclic).
4. **Farm-derive snapshot tests** — the `KindStr` adoptions rely on the farm's
   own `quote!→syn::parse2→prettyplease` + `assert_tokens_contain!` verification
   (already run on the farm side); the sui side only byte-pins the *behavior*.
5. **Forcing-function fixtures** — P1/P2 ship a compile-fail fixture proving a new
   opcode without a byte/operand column fails to compile.

---

## Whether any new tatara-rust-ast farm derive must be published first

**No.** The Layer A candidates (`HashAlgorithm`, `ContentAddressMethod`,
`Platform`) all fit the **already-published** `pleme-kindstr-derive`. The Layer B
prizes (`OpCode`, `WorkerOp`, operand arity) are **enum-specific byte/wire tables
whose values are load-bearing** — the correct mechanism is a **local
`macro_rules!`** in the owning crate (or extending sui-spec's `(defworker-opcode)`
TataraDomain), **not** a generic farm derive. Forcing them into a farm derive
would require the derive to carry arbitrary byte + operand-arity payloads per
variant — that is exactly a `(def…)` domain spec's job, which sui-spec already
does. **Execution can begin immediately at P0.**
