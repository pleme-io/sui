<!-- Authored by the sui→nix big-bang (/big-bang-pleme + /vocabularify), 2026-07-22.
     §0 (the three ratchets + the shipped ground) is operator-directive context;
     §1–§6 below are the adversarially-verified big-bang artifact. -->

# STRATOSPHERE — sui is the drop-in Nix

> Operator goal (verbatim): *"take every single possible use case nix can have and
> test it with sui and ensure sui has clear performance, clarity, beauty, and even
> if required optimization that only tatara-lisp can give for runtime fluidity in
> order to shoot its performance into the stratosphere."*

This is the definitive `/big-bang-pleme` + `/vocabularify` artifact for that
mission — genuinely multi-month, and treated as the large careful build it is:
the pinnacle named unhedged, "every possible nix use case" turned into a **typed,
CI-gated, enumerable vocabulary**, maximally reusing sui's shipped substrate, every
tier claim **adversarially verified against source** (the skeptic panel corrected
several overclaims — see §4's ✗-BUILD list and §6's open risks). §1–§6 are that
verified artifact. §0 below is the operating frame it runs inside.

---

## §0 — THE THREE RATCHETS (the no-regression system the mission leans on)

Aggressive optimization is only *safe* because progress is **monotonic by
construction**: three standing CI invariants each forbid sliding backward on one
axis, so the evaluator can be reshaped, flipped to the IR engine, and steered by
tatara-lisp knobs without ever regressing. Every move in §5's path rides all three.

1. **Byte-parity ratchet** — `sui parity` (CONVERGE=SEAL, `parity.yml`). A change
   that alters eval bytes goes red. *Never regress correctness.* (Held green through
   all 7 of this session's commits — verified byte-identical on nixpkgs
   `hello.drvPath` after the changes.)

2. **Coverage ratchet** — `coverage_at_100.rs` (`MAX_NON_WORKING`,
   `MIN_REPLACEMENT_PCT`, working-floor) + the new use-case matrix (§2). Coverage
   only ratchets *up*; a demotion must lower a floor **explicitly, in-commit, with a
   reason**. *Never silently regress coverage.* The vocabulary in §2 generalizes
   this: a use case with no coverage row — or a row claiming more than its evidence
   earns — fails the build.

3. **Perf ratchet** — `use-case-baseline.json` + the `perf-seal` gate + the
   `perf.rs` honesty ledger (a lever must show a strictly-positive `Delta`). The
   committed per-cell numbers become a **floor that only moves toward faster**; a
   measured cell crossing its floor fails CI, and an improvement ratchets the floor
   tighter. *Permits no performance regression, anywhere.*

   **Load-robust measurement is a required property of this ratchet, not a detail.**
   Wall-time under uncontrolled machine load produces *false* regressions — measured
   live 2026-07-22: nixpkgs `hello.drvPath` read 10–26 s wall under a 16-agent
   workflow (load avg ~20) while its true cost held at **~2.1 s user-CPU**, stable
   across every run; the baseline JSON records the same trap ("35.2 s under recon
   load vs 22.8 s quiet"). So the perf ratchet MUST key on a load-robust metric
   (user-CPU / instruction count) or enforce a quiet-measurement protocol (a
   load-average guard that defers the measurement) — otherwise "permits no
   regression" degrades into "cries wolf under load" and the ratchet the mission
   leans on erodes. This is a first-class design requirement of §5/M4's U-perf
   typed catalog.

**The compounding shape:** each closed use-case row is real ground the next stands
on, and the three ratchets guarantee the ground never crumbles under it — you can
only ever move forward. That is what makes an eternal-optimization mission joyful
rather than a treadmill: every increment is verified real, and no increment can
undo a prior one.

### Real ground already laid (2026-07-22 session — verified + committed)

Not plan, not claim — shipped and proven against reality:

- **S1 language corpus 25 → 114 passing real Nix tests** (23 honest known_broken gaps; vendored from CppNix's own
  `tests/functional/lang`, `.exp` regenerated from the local nix oracle). Two root fixes drove it: the **deepSeq cyclic-hang**
  (`deep_force` gained a cppnix-style `Rc`-identity seen-set — a hang→terminate is
  correctness *and* perf) and the **import-resolution cascade** (vendored shared
  support files + eval-with-file-path graduated 15 fixtures from one fix).
- **Coverage made honest** — the CLI catalog's inflated "77 Working" corrected to a
  truthful 65 Working / 10 Stub / 2 Partial (verified against `main.rs` handlers),
  100% → 84.4%, ratchets reconciled in-commit.
- **Tatara-lisp fluidity beachhead** — `eager_class` (sui-spec's 38th TYPED-SPEC
  domain): the `(defeager-class …)` knob + the **enforcement border** that refuses
  any knob whose technique isn't `ByteSufficient` — the parity-typed-knob law made a
  checked boundary, not prose.
- **Clarity** — `[workspace.lints]` hoisted, the 7 unlinted crates closed.

These are §5's pattern proven in miniature — the M0 vocabulary below is the same
TYPED-SPEC shape (border + `.lisp` + interpreter + catalog reflection) these already
follow.

---

## 1. DESTINATION (unhedged pinnacle)

**sui is Nix.** Not "mostly", not "for the shapes we tried" — a drop-in binary where every observable behavior a real operator or nixpkgs exercises is reproduced, and *we can prove which ones by construction*. The pinnacle is not "sui passes a lot of tests"; it is:

> **"Every nix use case tested" stops being prose and becomes a TYPED, mechanically-enumerable, CI-gated matrix in which a use case with no coverage row FAILS THE BUILD — and a coverage row that CLAIMS more than its evidence earns ALSO fails the build.**

The use-case space is the disjoint union of five surfaces — the language (S1), whole-closure byte-parity (S2), the CLI contract (S4), config+daemon+PATH (S5/S6/S7), and the twelve-class perf matrix (U-perf). The destination is one meta-catalog that (a) makes the *surface set itself* compile-exhaustive, (b) delegates each surface's *item set* down to a bijection gate against a reflection of sui's declared surface, and (c) welds every row to a `perf.rs`-style honesty seal so **"covered" is never a diligence property of the author — it is a structural property of the row.**

Two things the destination does NOT claim, stated up front so they can never be rounded away:

- **The matrix proves sui-INTERNAL name/shape coverage + internal honesty, not sui-vs-nix behavioral soundness.** The bijection gates reflect *sui's own declared surface* (`Commands::` in `main.rs`, the builtin registry, `worker_protocol.lisp`), NOT cppnix. Whether a covered row is *actually equal* to nix is a C2 external-oracle observation, forever runtime/CI, never a type. `coverage_at_100.rs` already proved a command graded `Working` can silently diverge from nix (`sui build --json` discards every flag). The matrix records "oracle wired", never "proven equal".
- **The S2 whole-closure theorem at cid scale (20,827 drvs) is CANNOT-COMPLETE** (U10 memory wall, ~154 MB/s retained-alloc, swap-death). No vocabulary removes that blocker. The matrix makes the blocker a first-class typed field; it does not close it.

The destination is therefore: **the gap between "sui runs" and "sui is Nix" becomes visible, tier-labelled, enumerable, and un-round-up-able.** Then we close it row by row, and each closed row is real ground the next stands on.

---

## 2. THE TYPED USE-CASE VOCABULARY (the vocabularify core)

The vocabulary is **two altitudes**, only one of which is genuinely new algebra. It is a synthesis: Design-1's meta-catalog for the **surface axis**, Design-3's claim-ledger honesty seal for the **row core**, Design-2's `CoverageRow` trait for the **item-axis union**. The `perf.rs` honesty machine is cloned wholesale — it is the deepest, most faithful, already-green precedent in the tree.

### Altitude 1 — the surface meta-catalog (genuinely compile-tier)

`sui-spec/src/nix_surface.rs` — a *second instance* of the shipped `catalog.rs::SubstrateDomain` meta-catalog, lifted one level: where `SubstrateDomain` enumerates domains-inside-sui-spec, `NixSurface` enumerates surfaces-of-nix.

```rust
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defnix-surface")]
pub struct NixSurface {
    pub id: Surface,                              // 1:1 onto the Surface enum
    pub covers: String,                           // one-line use-case-space statement
    #[serde(rename = "rowForm")]     pub row_form: String,   // "" = no per-item catalog yet
    #[serde(rename = "reflects")]    pub reflects: Reflects, // what sui-side surface the gate reads
    pub oracle: Oracle,                           // what a covered row is checked AGAINST
    pub tier: SurfaceTier,                        // claimed; honesty-gated vs earned
    pub blocker: SurfaceBlocker,                  // perf::Ceiling analog
    #[serde(default)] pub notes: String,          // the frontier, never hidden
}

pub enum Surface { S1Language, S2Closure, S4Cli, S5Config, S6Daemon, S7Path, UPerf }
// Surface::ALL: &[Surface] is a HAND-WRITTEN const (no strum, no allvariants derive in
// sui-spec/Cargo.toml — that reuse claim was false). The exhaustive match below is the gate.

pub enum Reflects {
    BuiltinRegistry,      // sui_eval builtins attrset keys (S1)
    SuiCommandsEnum,      // Commands:: scanned from main.rs (S4) — SUI's own, not nix's
    WireOpcodeCatalog,    // worker_protocol.lisp authored set, bridged to wire::WorkerOp (S6)
    NixShowConfigKeys,    // `nix show-config --json` (S5) — the ONE gate that reflects cppnix
    CppnixBinListing,     // pinned cppnix bin/ (S7) — also genuinely cppnix-side
    ClosureWalk,          // BuildClosure::compute node set (S2)
    UseCaseCatalog,       // the authored (defuse-case) set (U-perf) — self-referential, honest
    None,
}
pub enum Oracle { ByteParity, ExpFixture, ParityCheck, RealClient, PathResolve, PerfSeal, Absent }

#[derive(PartialOrd, Ord)]
pub enum SurfaceTier { Absent, Design, Enumerated, ParityWired } // Ord by decl order (ProofTier twin)
pub enum SurfaceBlocker { None, MemoryWall, NoParser, NoShims, HarnessOnlyNoOracle }

// earned_tier — the max tier the NAMED machinery honestly earns (perf::earned_tier twin)
pub fn earned_tier(row_form: &str, reflects: Reflects, oracle: Oracle) -> SurfaceTier {
    if row_form.is_empty() || reflects == Reflects::None { return SurfaceTier::Design; }
    if oracle == Oracle::Absent { return SurfaceTier::Enumerated; }
    SurfaceTier::ParityWired
}
```

**CRITICAL HONESTY CORRECTION (per all three verdicts):** `SurfaceTier::ParityWired` is deliberately NOT named `ParityProven`. It means "a live oracle is bound to the covered rows", never "proven equal to nix". `Reflects::SuiCommandsEnum` is documented in-type as *sui's own declared surface* — a real nix command sui has not implemented never appears in `main.rs` and produces no red. The matrix mechanically proves sui-internal coverage, not sui-vs-nix behavioral coverage. This is the single most-corrected overclaim; it is baked into the type names.

### Altitude 2 — the shared claim-ledger row core (Design-3, the honesty seal)

`sui-spec/src/coverage.rs` — a faithful transliteration of `sui-spec/src/perf.rs`. Every per-surface row NESTS one `claim: CoverageClaim` sub-form.

```rust
/// Witness — the Delta twin. Sole fallible ctor; a blank pointer is NOT a Witness.
/// Seals evidence PRESENCE purely. Whether the fixture PASSES is C2, coverage-seal, never a type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Witness { reference: String }
impl Witness {
    pub fn of(reference: &str) -> Option<Self> {
        let r = reference.trim();
        (!r.is_empty()).then(|| Self { reference: r.into() })
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GateTier { NoEvidence, Asserted, SmokeProbe, OfflineFixture, LiveParity }
pub enum EvidenceKind { None, ProseNote, SmokeProbe, OfflineFixture, ParityCheck }

// NAME-COLLISION NOTE: sui-spec/src/nix_replacement_coverage.rs ALREADY ships an
// `enum CoverageStatus`. This one is module-qualified (coverage::ClaimStatus) to avoid the
// clash — and §6 carries "fold nix_replacement_coverage into this ledger" as an open item.
pub enum ClaimStatus { Covered, KnownBroken, Unimplemented, Absent }
pub enum Ceiling { NotApplicable, NoNixEquivalent, OracleUnrunnableAtScale, OnlineOnly, ExternalObservation }

pub fn earned_gate(k: EvidenceKind) -> GateTier { /* total map, earned_tier twin */ }

pub enum CoverageViolation {          // the HonestyViolation twin; Display IS the message (no format!)
    CoveredWithoutWitness,            // ~ ProvenWithoutImprovement
    GateOverclaim { claimed: GateTier, earned: GateTier }, // ~ TierOverclaim
    WitnessWithoutEvidenceKind,
    AbsentWithoutCeiling,
}

#[derive(DeriveTataraDomain, Serialize, Deserialize, Clone)]
pub struct CoverageClaim {
    pub status: ClaimStatus, pub gate: GateTier,
    pub evidence: String, pub evidence_kind: EvidenceKind, pub ceiling: Ceiling,
}
impl CoverageClaim {
    pub fn witness(&self) -> Option<Witness> { Witness::of(&self.evidence) }
    pub fn honesty_violation(&self) -> Option<CoverageViolation> { /* perf.rs order */ }
    pub fn is_honestly_covered(&self) -> bool { self.honesty_violation().is_none() }
}

/// The item-axis union (Design-2). Every per-surface row impls this.
pub trait CoverageRow { fn surface(&self) -> Surface; fn id(&self) -> String; fn claim(&self) -> &CoverageClaim; }
pub fn all_rows() -> Result<Vec<Box<dyn CoverageRow>>, SpecError> { /* fold every load_canonical_* */ }

pub trait CoverageEnvironment { /* mockable seam — TYPED-SPEC triplet */ }
pub fn apply<E: CoverageEnvironment>(row: &dyn CoverageRow, env: &E) -> Result<Audit, SpecError> {
    // honesty FIRST — a dishonest row returns Err(SpecError::Interp{phase:"coverage-honesty"}),
    // NEVER a silent Ok (the no-stub rule).
}
```

`ClaimStatus::Covered` REQUIRES `witness().is_some()` exactly as `LeverStatus::Proven` requires `MeasuredKind::Improved`. A `:status Covered :evidence ""` row has no honest inhabitant — `every_coverage_claim_is_honest` fires `CoveredWithoutWitness`. This IS "never round up", live and mechanical.

### The `(def…)` forms

**Surface board** — `sui-spec/specs/nix_surface.lisp` (M0 honest state: 2 wireable, 5 honest Design/Absent):

```lisp
(defnix-surface :id S4Cli :covers "every sui subcommand's presence tracks its declared surface"
  :row-form "defsui-command" :reflects SuiCommandsEnum :oracle ParityCheck
  :tier Enumerated :blocker None
  :notes "command-NAME level SHIPPED + gated (cli_coverage 109 rows, Commands:: scan). This is sui's OWN declared surface, NOT cppnix's — name-presence, not behavioral parity. Per-flag/exit/json rows are a separate defsui-flag deliverable this row does not claim.")

(defnix-surface :id S6Daemon :covers "real cppnix clients drive sui over the worker protocol"
  :row-form "defworker-opcode" :reflects WireOpcodeCatalog :oracle RealClient
  :tier Enumerated :blocker HarnessOnlyNoOracle
  :notes "wire-SHAPE catalog SHIPPED (WorkerOpcode STRUCT — not an enum, no EnumIter). Bridge to the type the server actually dispatches on (sui_compat::wire::WorkerOp) is by STRING NAME. ~20/32 opcodes runtime-stub; real_nix_client is opt-in (silent no-op in CI).")

(defnix-surface :id S1Language :covers "every builtin/production/operator/error-path byte-matches cppnix"
  :row-form "defbuiltincase" :reflects BuiltinRegistry :oracle ExpFixture
  :tier Design :blocker None
  :notes "88/137 fixture harness SHIPPED but UNTYPED. defbuiltincase catalog + registry bijection is M0. GRAMMAR/OPERATORS have NO owned enum to enumerate — parsing is via foreign rnix::ast (rowan CST); strum is not a dep. deflangcase over grammar is a WEAK CI scan, never compile-tier.")

(defnix-surface :id S2Closure :covers "every drv in a system closure is byte-identical (ATerm + NAR)"
  :row-form "" :reflects ClosureWalk :oracle ByteParity :tier Design :blocker MemoryWall
  :notes "visit-all walk + NodeCoverage design. The cid 20,827-node run CANNOT COMPLETE — can never reach ParityWired at scale regardless of vocabulary.")

(defnix-surface :id S5Config :covers "every nix.conf/NIX_CONFIG/--option/NIX_PATH knob"
  :row-form "" :reflects None :oracle Absent :tier Absent :blocker NoParser
  :notes "NO parser exists. sui.yaml (SuiDaemonConfig) is orthogonal and MUST NOT be counted. The matrix records the gap; it cannot fill it.")

(defnix-surface :id S7Path :covers "every nix entrypoint binary resolves to a sui shim"
  :row-form "" :reflects CppnixBinListing :oracle PathResolve :tier Design :blocker NoShims
  :notes "only the unified `nix` shim ships; legacy nix-* unenumerated.")

(defnix-surface :id UPerf :covers "the 12 use-case perf classes at the G0/G1/G2 ladder"
  :row-form "" :reflects UseCaseCatalog :oracle PerfSeal :tier Design :blocker None
  :notes "perf-seal ratchet SHIPPED over eval SHAPES, not U-cells. defuse-case catalog + UseCaseClass enum do NOT exist. A coverage-% ratchet is a NEW schema, not the shipped work-budget Baseline. 3/12 honest G2 wins today.")
```

**Item row (M0 vertical, S1 builtins)** — `sui-spec/specs/builtin_case.lisp`:

```lisp
(defbuiltincase :name "genericClosure" :module convergence :arity 1
  :claim (:status Covered :gate OfflineFixture :evidence "eval-okay-genericClosure"
          :evidence-kind OfflineFixture :ceiling NotApplicable))
(defbuiltincase :name "getAttr" :module attrs :arity 2      ; the honest floor, made VISIBLE
  :claim (:status Covered :gate SmokeProbe :evidence "smoke:attrs"
          :evidence-kind SmokeProbe :ceiling NotApplicable))
(defbuiltincase :name "genericClosure'" :module convergence :arity 1  ; ~100 like this at M0
  :claim (:status Unimplemented :gate NoEvidence :evidence "" :evidence-kind None
          :ceiling NotApplicable))
```

The nested `:claim (...)` is FEASIBLE — verified against `tatara-lisp-derive` 0.2.2: any non-primitive field routes through `Kind::Deserialize` (`sexp_to_json` + `serde_json::from_value`, commented "Unlocks enums, nested structs, Vec<Struct>"); `perf.rs` already loads bare-symbol unit enums, and `activation_script.rs::ActivationPhase` already proves the nested-sub-form shape.

### The CI forcing-function (three gates, two altitudes, tier-labelled)

**SURFACE AXIS — genuinely compile-tier (the ONE truly-unrepresentable slice).**
`sui-spec/tests/nix_surface_matrix.rs::matrix_covers_all_surfaces`:
```rust
for s in Surface::ALL { match s { Surface::S1Language => …, /* every arm */ } } // `_ =>` BANNED
assert_eq!(rows.len(), Surface::ALL.len());
```
A new `Surface` variant with no arm does not COMPILE; a dropped row fails the assert. You cannot add a use-case surface and forget to account for it.

**ITEM AXIS — C1 CI forcing-function (RED, not compile).**
`every_gated_surface_has_a_live_gate`: for each row with `tier >= Enumerated`, dispatch on `reflects` and DELEGATE to that surface's own bijection gate:
- `SuiCommandsEnum` → the SHIPPED `cli_coverage_invariants.rs::every_cli_subcommand_has_a_catalog_entry` (word-boundary `Commands::` scan + shrink-only `UNCATALOGUED` escape). Reflects **sui's `main.rs`**, not cppnix.
- `WireOpcodeCatalog` → the SHIPPED `worker_protocol.rs::tests::essential_opcodes_present` (a hardcoded 10-name presence spot-check — NOT a reflection of cppnix's `worker-protocol.cc`; both sides derive from `worker_protocol.lisp`, so it is self-consistency, honestly labelled).
- `BuiltinRegistry` (once M0 ships) → `every_builtin_has_a_coverage_row`: build the real registry, bijection registry-keys↔catalog.

**HONESTY GATE — the load-bearing one.**
`every_coverage_claim_is_honest` (the `perf.rs::every_authored_lever_is_honest` twin): `for row in all_rows() { assert!(row.claim().is_honestly_covered()) }` and `every_surface_is_honest` (`claimed <= earned_tier(...)` or `TierOverclaim`; a runnable-but-uncovered surface must name a `blocker`). A row cannot paint itself green without the reflection source + oracle actually existing.

**BACKSTOP (four hand edits, NOT one free line — verdict correction):** register `(defsubstrate-domain :name "nix_surface")` and `"coverage"` in `catalog.lisp`, add the required-name-list entry in `catalog.rs::every_authored_domain_is_in_catalog`, and add a match arm in `substrate_invariants.rs::every_catalog_entry_has_a_loadable_module` (which ends in a runtime `other => panic!`, red at test-time, not compile-time).

---

## 3. THE 5 SURFACES — tier-honest coverage + typed-catalog path

| Surface | Shipped coverage (un-rounded) | Reflection source | How it becomes a typed catalog | Honest ceiling |
|---|---|---|---|---|
| **S1 Language** | Corpus **114/137 ≈ 64%** eval-okay fixtures (`lang_corpus.rs`, 114 active + 23 `known_broken/`). Builtins **~89/120 ≈ 74% IMPLEMENTED**; per-builtin PARITY far thinner — 19 module-level smoke probes only, **~100 builtins have zero dedicated coverage**. Grammar: all 25 productions dispatched, **none enumerated**. | Builtin registry keys (must build `builtin_names()`). **Grammar has NO owned enum** — rnix `ast` is foreign rowan CST; `sui_ir::ir::{BinOp,UnaryOp}` exist but are "wired into nothing". | `(defbuiltincase)` (M0) + `(deflangcase)` for productions/operators. Builtin bijection is a strong registry gate; grammar is a WEAK name-scan only (no compile-tier claim). | Byte-parity soundness is C2 (fixture `.exp` correctness untrusted). Per-production compile-tier enumeration is INFEASIBLE (foreign AST). |
| **S2 Closure** | **~0% enumerated.** `bisect_drv` (`main.rs:4763`) is DFS-to-first-leaf, operator-only. `build-parity.yml` = 2-row NAR scaffold. Walk substrate ~80% ready (`BuildClosure::compute`, `closure.rs:32`). | `BuildClosure::compute` node set over both toplevels. | `(defclosure-subject)` + machine-walked `NodeCoverage` rows + `Divergence::MissingNode`. One code change: replace `bisect_drv`'s first-child descent with a visit-all walk reusing the `parsed` memo. | **CANNOT-COMPLETE at cid scale** (memory wall, swap-death). Row carries `blocker=MemoryWall`; never reaches `ParityWired` at 20,827 nodes. |
| **S4 CLI** | Command-NAME level **100%** (109 `defsui-command` rows + `Commands::` scan, green today). Flag/exit/JSON contract **~0% typed** — 165 `#[arg]` defs live only as English in 16 `:notes`. | `Commands::` (sui's own source) for names; a NEW `#[arg]`-ident scanner for flags. | Extend `SuiCommand` with `flags: Vec<SuiFlag>` (`#[serde(default)]`, non-breaking) + `ContractAssertion`. New gate `every_arg_has_a_covered_flag_row`. | Name presence ≠ behavioral parity (`coverage_at_100.rs`: `sui build --json` discards flags). The flag scanner is genuinely NEW — the shipped scan matches subcommand VARIANTS, not `#[arg]` attrs. |
| **S5 Config** | **NONE. 0 rows, no parser.** `nix.conf`/`NIX_CONFIG`/`--option`/`NIX_PATH` never parsed. `SuiDaemonConfig` (sui.yaml) is orthogonal — MUST NOT be counted. | `nix show-config --json` keys (the one genuinely cppnix-side reflection). | `(defnix-setting)` + `(defsearch-path-entry)` — but rows sit honestly `Absent` until a parser exists. | The parser is the largest NEW build. No `format!()` — a typed setting AST. |
| **S6 Daemon** | Server is REAL (`DaemonServer::run`, 12/32 opcodes handled). Catalog models wire-SHAPE only (`WorkerOpcode`, direction+shape), **no server-status field**, **~20 opcodes runtime-stub**. Only oracle (`real_nix_client.rs`) is opt-in, silent no-op in CI. | `worker_protocol.lisp` catalog, bridged **by string name** to `sui_compat::wire::WorkerOp` (a DIFFERENT enum, DIFFERENT crate, that `dispatch.rs` actually matches on). | Add `server_status: {Handled,AckOnly,TypedError,Unimplemented}` to `WorkerOpcode`; promote `real_nix_client.rs` from opt-in to gated. | `WorkerOpcode` is a STRUCT with no `EnumIter` (strum not a dep) — "reflect its EnumIter" was a false claim. The bridge is name-based. |
| **S7 Path** | Shimmed for **1 entrypoint (`nix`)** with a real lock gate (`lock_100_percent.rs`). Legacy `nix-*` unenumerated; 0 PATH-shadow rows. | Pinned cppnix `bin/` listing. | `(defpath-entrypoint)` + a `which <name>`-resolves-to-shim probe + shims for legacy binaries. | `nix-wrap.nix` header is stale (says "fall back to cppnix"; `main.rs` is now no-fallback exit-78). |
| **U-perf** | As TYPED catalog: **0/12**. `UseCaseClass` enum + `use_case_matrix.rs` + `matrix_covers_all_use_case_classes` **do NOT exist** (confirmed absent). As EVIDENCE (CORRECTED 2026-07-22, load-robust user-CPU): **U04 is NOT a win** — byte-identical but **~7× SLOWER** in real CPU (sui ~2.15 s vs nix ~0.30 s; the prior "1.4× faster" was a wall-time artifact — nix's warm wall is mostly cache-read wait, sui's wall is multithreaded compute). Both sui engines are equally slow (the cost is the shared rowan-re-walk + im\_rc env-COW substrate → the L3-IR/M5 target). U01 (tiny) + U02 (warm eval-cache) remain wall-based wins, user-CPU-unverified; U03 tie; U05 9.4× LOSS; U10 G0-RED DNF; rest unmeasured. Honest count: **0/12 confirmed load-robust CPU wins vs nix**; the stratosphere target is quantified as driving U04 user-CPU 2.15 s → nix's 0.30 s. | The authored `(defuse-case)` set (self-referential — honest). | `(defuse-case)` per `UseCaseClass` variant; reuse `perf::{Delta, MeasuredKind}` so a "G2 at ≤1.0×" cell is unconstructable. | The coverage/gate ratchet is a NEW schema (the shipped `perf_seal` Baseline is a per-row EvalExpr work-budget, not a %-floor). Rebinding the runtime matrix to typed `Delta` is a re-architecture, not free reuse. |

---

## 4. REUSE MAP (strict — over-claims caught by the verdicts are marked ✗ BUILD)

**REUSE VERBATIM (shipped, verified present):**
- **Authoring/loading pipeline** — `DeriveTataraDomain` + `#[tatara(keyword=…)]` + `#[serde(rename/default)]` + `include_str!(specs/*.lisp)` + `loader::load_all::<T>` → `load_canonical()`. Every domain instantiates it (`catalog.rs:24`, `eager_class.rs:41`, `perf.rs:242`, `loader.rs:18`).
- **The perf.rs honesty machine, WHOLESALE** — `Delta` sole-fallible seal (`perf.rs:82`), `earned_tier` (`:269`), `HonestyViolation` Display-is-message no-`format!` (`:284`), `apply<E>` returns typed `SpecError::Interp` never silent-Ok (`:425`), `every_authored_lever_is_honest` + `assert_eq!(len)` (`:504`). `Witness`/`earned_gate`/`CoverageViolation` are a line-for-line reshaping.
- **`perf::{Delta, MeasuredKind}` CONSUMED DIRECTLY** by the future `UseCaseRow` speedup axis — sign-unrepresentability for free.
- **The meta-catalog pattern** — `catalog.rs::SubstrateDomain` + `every_authored_domain_is_in_catalog` + `substrate_invariants.rs::every_catalog_entry_has_a_loadable_module`. `NixSurface` is a second instance one altitude up.
- **S4 down-delegation** — `cli_coverage.rs::SuiCommand` (109 `defsui-command` rows, `grep -c` = 109) + `every_cli_subcommand_has_a_catalog_entry` (word-boundary `Commands::` scan + shrink-only `UNCATALOGUED`). **Reflects sui, not nix.**
- **S6 down-delegation** — `worker_protocol.rs::WorkerOpcode` + `essential_opcodes_present` (hardcoded 10-name spot-check).
- **Oracles** — `parity.rs::{ParityCheck, Verdict, ShadowReport}`, `perf_seal.rs` ratchet mechanics, `lang_corpus.rs` 88+49 fixtures as per-row `OfflineFixture` oracle (ZERO new fixtures), `NixAttrs::keys` (`value.rs:2369`).
- **`eager_class.rs`** — the freshest single-struct + `validate()`-border precedent, and it already CONSUMES `perf::{earned_tier, ProofTier}` — proof the consume-perf pattern is in-tree.
- **`SuiCommand` uses `#[serde(default)]`** (`cli_coverage.rs:49`) — adding `:flags` is non-breaking.

**✗ BUILD (genuinely new — the verdicts caught these cited as reuse):**
- ✗ `sui_eval::builtin_names()` — **does NOT exist.** Must build. Correct signature: top-level `register(env: &mut Env)` (`builtins/mod.rs:168`) populates an `Env`, NOT a `NixAttrs`; the accessor must extract the builtins attrset from the `Env` then call `NixAttrs::keys`.
- ✗ `ast::Expr` / `Production` / `BinOpKind` `EnumIter` — **NO owned enum, strum not a dep.** Parsing is foreign `rnix::ast` (rowan CST). Grammar cannot be compile-tier enumerated. `sui_ir::ir::{BinOp,UnaryOp}` exist but are the unwired lowered IR.
- ✗ `WorkerOpcode` `EnumIter` — it is a `#[derive(DeriveTataraDomain)]` STRUCT, no enum, no strum. Bridge to `sui_compat::wire::WorkerOp` (`sui-compat/src/wire.rs:31`, the type `dispatch.rs:47` matches) is by string name.
- ✗ `matrix_covers_all_use_case_classes` / `UseCaseClass` enum — do NOT exist (zero hits). The U01–U12 matrix is prose (`benches/USE-CASE-MATRIX.md`).
- ✗ "macro-farm `allvariants` derive for `Surface::ALL`" — no such derive used in sui; `Surface::ALL` is a hand-written const.
- ✗ A coverage-% ratchet from "the shipped perf_seal Baseline shape" — the Baseline is a per-row `Budget{eval_expr}` work-budget with a tolerance band. A %-floor is a new schema.
- ✗ S4 flag scan "reuse the `#[arg]` scan" — the shipped scan matches subcommand VARIANT names, not `#[arg]` attrs. The flag scanner is new.
- ✗ `matrix_covers_all_surfaces` as "the same move as backstop compile-time" — the `Surface` match IS compile-tier; the catalog backstop is runtime `panic!` (red, not compile).

**PRIOR OVERLAP (must cite, do not collide):** `sui-spec/src/nix_replacement_coverage.rs` ALREADY ships a workload-coverage catalog with its own `enum CoverageStatus` and an `owns` evidence-pointer (a weaker shipped `Witness`). §6 carries "fold it into the `coverage.rs` ledger" as an open item; until then `coverage::ClaimStatus` is module-qualified to avoid the clash.

---

## 5. THE PHASED PATH M0..Mn

**M0 — the S1 builtin vertical, end-to-end (proves the vocabulary against ONE real surface with ZERO blocked deps).** Chosen because its authority is fully shipped, PURE, offline, and free of every scaling/oracle wall (no network like S5, no memory wall like S2, no absent parser). One commit:
1. `sui-spec/src/coverage.rs` — the shared honesty core (`CoverageClaim` + `Witness` + `GateTier`/`EvidenceKind`/`Ceiling`/`ClaimStatus` + `earned_gate` + `CoverageViolation` + `honesty_violation`/`is_honestly_covered` + `apply<E>`). A direct `perf.rs` transliteration.
2. `sui-spec/src/nix_surface.rs` + `specs/nix_surface.lisp` — the 7-row surface board (2 `Enumerated`, 5 honest `Design`/`Absent`) + `matrix_covers_all_surfaces` (compile-exhaustive) + `every_surface_is_honest`.
3. `sui-spec/src/builtin_case.rs` + `specs/builtin_case.lisp` — `BuiltinCase{name,module,arity,claim}`, `#[tatara(keyword="defbuiltincase")]`, one row per registered builtin (~89), most honestly `SmokeProbe`/`Unimplemented` — the true ~74%/thin-parity floor made VISIBLE, not rounded.
4. `sui-eval/src/lib.rs::builtin_names()` — the one missing accessor (wrap `register(&mut Env)` + extract builtins attrset + `NixAttrs::keys`).
5. `sui-spec/tests/coverage_invariants.rs` — `every_builtin_has_a_coverage_row` (registry↔catalog bijection, `cli_coverage` template, shrink-only escape) + `every_coverage_claim_is_honest`.
6. `catalog.lisp` + `catalog.rs` required-list + `substrate_invariants.rs` arm (the four backstop edits).

*Green M0 proves, offline + mechanically:* (a) a `builtins.insert` with no row goes red; (b) `:status Covered :evidence ""` goes red (`CoveredWithoutWitness` — never-round-up, live); (c) `:gate LiveParity :evidence-kind ProseNote` goes red (`GateOverclaim`); (d) `apply()` errors on a dishonest row. It surfaces the true number (~89 registered vs ~120 cppnix vs ~20 with a real per-builtin fixture) as DATA, not prose. **Green M0 proves the honesty MACHINE works — it does NOT prove nix is covered. Those are different claims and the vocabulary is built so they cannot be conflated.**

**M1 — S4 flag contract.** `SuiFlag`/`FlagKind`/`ContractAssertion` nested in `SuiCommand`; the new `#[arg]`-ident scanner; `every_arg_has_a_covered_flag_row`. Cheapest next surface (source authority already exists). Flip S4's board row from name-only to flag-covered.

**M2 — S1 grammar/operators (weak scan tier) + S6 daemon server-status.** `(deflangcase)` over productions (name-scan only, no compile-tier claim — honestly labelled); `server_status` field on `WorkerOpcode` + name-bridge to `wire::WorkerOp` + promote `real_nix_client.rs` to a gated conformance corpus.

**M3 — S7 PATH catalog.** `(defpath-entrypoint)` + PATH-shadow probe + legacy `nix-*` shims; fix the stale `nix-wrap.nix` header.

**M4 — U-perf typed catalog.** `UseCaseClass` (12 variants) + `(defuse-case)` + `use_case_matrix.rs` (`matrix_covers_all_use_case_classes`, `_ =>` banned + `assert_eq!(12)`) reusing `perf::Delta`; the NEW coverage/gate ratchet schema; the missing U07–U12 oracle captures.

**M5 — S2 closure walk (bounded).** Visit-all generalization of `bisect_drv` + `NodeCoverage` + `Divergence::MissingNode` + `(defclosure-subject)`; run against a SMALL pinned closure (leaf drvPath), NOT cid — the cid subject stays `CannotComplete/MemoryWall`.

**M6 — S5 config parser.** The genuinely-large new build: `nix.conf`/`NIX_CONFIG`/`--option`/`NIX_PATH` typed parser + `(defnix-setting)` + `nix show-config --json` oracle adapter. Flip S5's board row `Absent`→`Enumerated`.

**The compounding law across every phase:** the moment a per-surface catalog ships, its `NixSurface` row flips up a tier — and `every_surface_is_honest` REFUSES the flip until a real bijection gate for that surface exists. The matrix mechanically pulls each surface's own gate into being, one honest tier at a time.

---

## 6. TIER-HONEST LEDGER + OPEN RISKS

**SHIPPED (green today, verified against source):** `perf.rs` honesty ledger (20 levers, CI-gated); `cli_coverage.rs` (109 `defsui-command` rows) + `every_cli_subcommand_has_a_catalog_entry` (`Commands::` scan); `worker_protocol.rs::WorkerOpcode` + `essential_opcodes_present`; `lang_corpus.rs` 88/49 fixtures; `perf_seal` work-budget ratchet; `eager_class.rs` (perf-consuming precedent); `NixAttrs::keys`; `parity.rs` oracle; `BuildClosure::compute`; `nix_replacement_coverage.rs` (a weaker prior coverage catalog — must be folded in, not duplicated).

**DESIGN (this artifact — nothing of it exists yet):** `nix_surface.rs`, `coverage.rs`, `builtin_case.rs`, their `.lisp` catalogs, the three gate tests, `builtin_names()`. It is a transliteration of proven-green code, which is why M0 is small — but it is DESIGN. Only **2 of 7 surfaces (S4, S6) are wireable at `Enumerated` today.**

**ASPIRATIONAL (out of scope, shipped nowhere):** the C0 ceiling — macro-generating each per-item catalog FROM the registry/enum/clap-Command-model so a missing row is truly unrepresentable rather than red. The shipped gates concede this in their own headers ("a CI forcing-function (C1 ceiling), not a type … only makes it red"). This vocabulary inherits that ceiling.

**Tier of the gate itself:** the SURFACE axis is genuinely compile-tier (`_ =>`-banned `Surface` match — truly-unrepresentable). The ITEM axis is C1 CI forcing-function (red, not compile). **The grammar surface is NOT compile-tier** — no owned AST enum, foreign rnix, no strum; it is at best a weak name-scan.

**Open risks (surviving adversarial critiques, carried not hidden):**
1. **Name-coverage ≠ behavioral coverage.** Every wired gate reflects sui's OWN declared surface, not cppnix. A "Working"/"Covered" command can silently diverge from nix (`coverage_at_100.rs` proof). The matrix records sui-internal completeness + honesty; the C2 byte/behavioral soundness stays a separate, forever-external check. `SurfaceTier::ParityWired` (not `Proven`) encodes this.
2. **Witness seals presence, not truth.** A row citing a fixture that tests the WRONG builtin passes `is_honestly_covered` while being unsound — the type gates KIND and PRESENCE, never evidence faithfulness (perf.rs's own Delta-vs-parity concession, carried).
3. **Fixture-liveness = existence, not exercise.** The M0 gate checks the `.nix`/`.exp` pair exists (and is not in `known_broken/`); it does not prove the fixture exercises the named builtin. "Checked, never asserted" is too strong — the binding→subject link is hand-authored.
4. **S6 self-referential authority.** `essential_opcodes_present` is a 10-name spot-check against the same `worker_protocol.lisp` both sides derive from — self-consistency, not a reflection of cppnix's `worker-protocol.cc`. Honestly labelled `HarnessOnlyNoOracle`.
5. **S2 is CANNOT-COMPLETE at scale.** The 20,827-node cid theorem is unrunnable (memory wall). The vocabulary TYPES the dead end (`blocker=MemoryWall`); it does not resolve it.
6. **S5 has no parser at all.** Every setting row is honestly `Absent` until M6. `sui.yaml` MUST NOT be miscounted as coverage.
7. **The numbers, un-rounded.** S1 is 64% corpus (88/137) and ~74% builtins implemented (~89/120), with per-builtin parity FAR thinner (~100 builtins zero dedicated evidence). At M0 most `BuiltinCase` rows will honestly be `SmokeProbe`/`NoEvidence`, NOT `LiveParity`. **Do not round any of this up to "the language is covered."**

The matrix makes the gap visible, tier-labelled, and un-round-up-able. It does not close it. Closing it is the M0..M6 path — and each closed row is real ground.