# The Byte-Parity Typescape — dominating sui↔nix byte-for-byte, forever

> **This is THE DOMINATION REFLEX applied to consuming nix** (org-level ★★★).
> New problem space (byte-parity) → dominate with re-usable standards (extend
> `sui-spec`, `sui-compat`, the probe machinery) → make all bad states invariant
> (a divergence is a red gate, unrepresentable as "green") → typescape the
> invariant knowledge (typed `def…` vocabulary + interpreter) → lock with an
> all-variants matrix. The output is a **suite** that makes "a byte differs"
> permanently impossible to ship, not a one-off check.

## Destination (name it first, unhedged)

`sui == nix`, **byte for byte, across the entire ecosystem** — nixpkgs →
darwin-nix → nixos → home-manager. For every subject in a growing corpus, sui's
`outPath`, `drvPath`, `.drv` ATerm, NAR, and realized store bytes are **identical
to nix's**, and that identity is a **theorem a CI gate re-proves on every commit**.
100% is table stakes (sui `CLAUDE.md` north-star); silent divergence — a wrong
value that still evaluates — is the worst failure and is made unrepresentable as
an acceptable state.

## The honest starting line (measured 2026-07-10, not asserted)

On an identical pinned nixpkgs source, `hello.drvPath` **and** `hello.outPath`
differ from nix; a 10-package corpus (coreutils, gzip, xz, jq, …) is **0/10**
byte-matching, and several pick the wrong *output* (nix `bzip2-…-bin` vs sui
`bzip2-…`). A bare `(derivation {…}).outPath` **does** byte-match nix — so sui's
core drv hashing is correct and the divergence is in **how nixpkgs computes the
derivation through the stdenv bootstrap** (`sui-spec/derivation.rs`'s
`SerializeModulo` / input-refs / modulo memoization is the first suspect). The
job below turns "0/10, unknown why" into "a corpus gate that is green or names the
exact differing byte."

## Status — M0 realized (2026-07-10): the sealed corpus gate is live
`sui parity` is now the all-variants corpus differential + gate (`main.rs`
`cmd_parity`). It carries the **xfail matrix** (`Expect::{Match, KnownDiverge}`):
a `Match` row that regresses OR a `KnownDiverge` row that *graduates to match*
fails the gate (`exit 1`) — divergence can neither regress nor silently advance
(CONVERGE=SEAL, the honest CI-caught tier, since the oracle is an external
process). **Eval-parity probes** (`diff_eval`: `sui --no-vm eval` vs `nix eval`,
byte-for-byte) cover the mission core; seeded green with `builtins.placeholder`,
`(derivation{…}).outPath`, and the FOD `.drvPath`. The ecosystem target
`(import <nixpkgs> {}).hello.drvPath` is a tracked `KnownDiverge` row that prints
both differing drvPaths — the exact next root (stdenv-bootstrap drv computation);
it auto-graduates the gate the moment parity lands. Extending parity = **add a
corpus row**, not hand-probe. Live: 11 probes, 10 match / 1 tracked / 0 regressions.
The `#[derive(DeriveTataraDomain)]` + `(defparity-artifact …)` typescape below is
the *next* tier (authorable corpus in Lisp); M0 ships the sealed gate in Rust.

## The typed vocabulary (the typescape)

Every concept is a `#[derive(DeriveTataraDomain)]` border + a `(def…)` tatara-lisp
surface + an `apply` interpreter behind a mockable `Environment` — the sui-spec
triplet, extending the existing `parity` domain (probe-based today) into a
**corpus-and-artifact** domain.

### 1. `ParityArtifact` — the byte-comparable unit — `(defparity-artifact …)`

```rust
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defparity-artifact")]
pub struct ParityArtifact {
    pub name: String,
    pub kind: ArtifactKind,       // what bytes we extract
    pub compare: CompareMode,     // how "equal" is defined (default ByteExact)
    pub tags: Vec<String>,
}

pub enum ArtifactKind {
    OutPath,        // .outPath store path (bytes of the string)
    DrvPath,        // .drvPath
    DrvAterm,       // the .drv file's ATerm bytes (via sui-compat Derivation)
    Nar,            // `store dump-path` NAR bytes of a realized path
    StorePathHash,  // the 20-byte digest
    Hash,           // a builtins.hash* result
    SystemClosure,  // config.system.build.toplevel.outPath (darwin/nixos/HM)
}

pub enum CompareMode { ByteExact, JsonCanonical, Sha256OfBytes }
```

`ByteExact` is the default and the only one that proves the destination; the
others exist only for artifacts where nix itself is non-deterministic in
whitespace (declared per-artifact, never a silent relaxation).

### 2. `ParitySubject` — one comparison, apples-to-apples by construction — `(defparity-subject …)`

The lesson that cost us today (comparing across different nixpkgs revs is
meaningless) is **encoded in the type**: a subject carries its `PinnedEnv`, and a
divergence report can only be built from a sui-result and a nix-result that share
the **same** `PinnedEnv`. Cross-pin comparison is unrepresentable.

```rust
#[derive(DeriveTataraDomain, …)]
#[tatara(keyword = "defparity-subject")]
pub struct ParitySubject {
    pub name: String,
    pub expr: String,              // the nix expression, e.g. `<pkgs>.hello`
    pub artifacts: Vec<String>,    // ArtifactKind names to compare
    pub pin: PinnedEnv,            // nixpkgs store path + system + impureness
}

pub struct PinnedEnv { pub nixpkgs: StorePathRef, pub system: String }
```

### 3. `ParityCorpus` — a named, growing set — `(defparity-corpus …)`

```rust
#[derive(DeriveTataraDomain, …)]
#[tatara(keyword = "defparity-corpus")]
pub struct ParityCorpus {
    pub name: String,              // "nixpkgs-core", "darwin-cid", …
    pub pin: PinnedEnv,
    pub subjects: Vec<ParitySubject>,
    pub required_artifacts: Vec<ArtifactKind>,  // the gate's floor
}
```

### 4. `Verdict` / `Divergence` — bad states have no "green" code path

Extends the existing `parity::Verdict`. A **confirmed parity** is a proof-carrying
value you cannot construct without the byte comparison having run and passed:

```rust
/// Constructed ONLY by `ParityEngine::compare` on a true byte-equal.
pub struct ByteMatch { artifact: ArtifactKind, blake3: [u8; 32] /* of both sides */ }

pub enum Divergence {
    Bytes  { kind: ArtifactKind, sui: Vec<u8>, nix: Vec<u8>, first_diff_at: usize },
    Output { kind: ArtifactKind, sui_outputs: Vec<String>, nix_outputs: Vec<String> },
    SuiError { kind: ArtifactKind, message: String },   // sui crashed/UndefinedVar
    Localized(DrvFieldDivergence),                      // bisected — see below
}

/// The drv-ATerm bisector's typed output — which field / input first diverges.
pub enum DrvFieldDivergence {
    Name, Builder, Args, System,
    Env { key: String },
    InputSrc { path: String },
    InputDrv { drv: String, inner: Box<DrvFieldDivergence> },  // recurse upstream
    Outputs,
}
```

`SystemReport` is `Vec<Result<ByteMatch, Divergence>>`; the gate is green iff
every result is `Ok`. There is no `Verdict::ProbablyFine`.

### 5. The interpreter + the mockable seam

```rust
pub trait ParityOracles {
    fn sui_eval(&self, subject: &ParitySubject, k: ArtifactKind) -> Result<Vec<u8>, OracleErr>;
    fn nix_eval(&self, subject: &ParitySubject, k: ArtifactKind) -> Result<Vec<u8>, OracleErr>;
}
pub fn apply(corpus: &ParityCorpus, o: &impl ParityOracles) -> CorpusReport;
```

Real impl drives `sui eval` + `nix eval --impure` via the typed `Command`
builders in `cli.rs` (NO SHELL) and `sui-compat` for NAR/drv byte extraction;
tests drive a mock `ParityOracles` (the Environment seam — the TYPED-SPEC triplet
testability contract).

### 6. The drv-ATerm bisector — `Divergence::Bytes` → root cause, mechanically

Given a `DrvPath` divergence, parse both `.drv`s (`sui_compat::Derivation::parse`),
compare field by field in canonical order (name, builder, args, system, env,
inputSrcs, inputDrvs); on an `inputDrvs` diff, **recurse into the first differing
input derivation**. This turns "hello differs" into "the divergence bottoms out at
`bootstrap-tools`'s env key `X`" — the single root that cascades to every package.
This is the tool that makes the stdenv divergence findable instead of infinite.

## The invariant + the gate (the "forever")

`sui parity corpus <name> --gate` returns non-zero on **one differing byte**. Wired
as a CI job on the sui repo, `sui == nix` over the corpus becomes a theorem
re-proven every commit. **A green build may never outrun the corpus it proves** —
coverage grows with the evaluator (§Phases). This is the enforced form of the
north-star hard fact.

## The all-variants proof (CLOSED-LOOP MASS-SYNTHESIS)

`tests/parity_matrix.rs`: one row per `ArtifactKind × CompareMode`, each exercised
against a fixture; plus `matrix_covers_all_kinds()` that **fails to compile / fails
the test when a new `ArtifactKind` lands without a matrix row** (exhaustive `match`
over the enum → no silent gap). The matrix proves the platform is ready for *all*
artifact variants, not the ones we happened to try. New corpus / new subject = a
`(def…)` line, never new engineering (Ship the suite, not the fix).

## Catalog entry (CATALOG REFLECTION)

```lisp
(defsubstrate-domain
  :name               "parity_corpus"
  :authoring-keywords ("defparity-artifact" "defparity-subject" "defparity-corpus")
  :gate               M3TypedOnly
  :purpose            "Byte-parity corpus — typed artifacts/subjects/corpora proving sui == nix byte-for-byte, gated"
  :cppnix-mirror      "n/a — pleme-io native"
  :depends-on         ("parity" "derivation" "nar" "store_layout" "hash"))
```

## Reuse map (Dominate with re-usable standards — Care #4)

| Layer | Reuse (do not rebuild) |
|---|---|
| Triplet + macros | `sui-spec` `Spec`/`TataraDomain` + `DeriveTataraDomain`; `loader::load_all` |
| Probe engine | `parity::ParityCheck`/`Verdict`/`ProbeContext`; the `sui parity` sweep |
| Byte primitives | `sui-compat::{Derivation, NarReader/Writer, StorePath, nix_base32_*}` |
| Oracle invocation | `cli.rs` `nix_cli::eval_expr` / `sui_cli::eval_expr` (typed `Command`, NO SHELL) |
| Catalog | `catalog.rs` topological registry + the invariant tests |
| Drv computation under test | `sui-eval/builtins/derivation.rs` → `sui-spec/derivation.rs` phases |

New code owned by this domain: the corpus/artifact/subject borders, the
`Divergence`/`ByteMatch` sum types, the `apply` interpreter, the bisector, the
matrix, the `--gate` CLI arm. Everything else is composition.

## Tier-honest ledger

| Item | Tier |
|---|---|
| 7 primitive probes (hash/NAR/drv round-trip) | **shipped** (`sui parity`, 7/0) |
| Bare `(derivation{}).outPath` byte-matches nix | **shipped** (core hashing correct) |
| `PinnedEnv` making cross-pin comparison unrepresentable | **design** |
| `ParityArtifact/Subject/Corpus` typed border + Lisp + catalog | **design** (this doc) |
| `Divergence`/`ByteMatch` proof-carrying sum types | **design** |
| Drv-ATerm bisector | **design** — the first tool to build (finds the stdenv root) |
| `--gate` CI corpus gate | **design** |
| all-variants matrix | **design** |
| nixpkgs package byte-parity | **RED, 0/N** — the stdenv drv divergence is target #1 |
| darwin/nixos/HM system-closure parity | **blocked** on the M2.6 fixpoint fix (`docs/M2.6-…`) |

## Phased plan (destination first, then the path down)

- **M0 — the bisector + one true corpus.** Build `ParityArtifact/Subject/Corpus`
  + the `apply` interpreter + the drv-ATerm bisector; add a `nixpkgs-core` corpus
  (the 10 packages, `DrvPath` + `OutPath`, `ByteExact`) + the matrix. First run
  reports the root drv-field divergence. **Gate: the bisector names the first
  differing `.drv` field in the stdenv chain.**
- **M1 — close the stdenv root.** Fix the derivation-computation divergence at its
  source (`SerializeModulo`/refs/modulo memo per the bisector) via the sui rhythm;
  regression test. Package corpus flips toward green; `--gate` goes live in CI.
- **M2 — widen the corpus.** Whole-nixpkgs sampling; NAR + realized-store parity
  (needs `sui build`); multi-output correctness.
- **M3 — system closures.** darwin-nix / nixos / home-manager `SystemClosure`
  parity (after M2.6). The gate spans the ecosystem; the flip is provable.

## Frontier finding (2026-07-10) — the libxcrypt/perl fixpoint-reentry root

Byte-verified against the live nix oracle (nixpkgs pin
`4dp7jwjpwb9filsqnrq7x7lw3kzbzkdk`). Cracked with the two diagnostics
`SUI_DUMP_DRV=<name>|all` (dumps a computed drv's inputDrvs/inputSrcs/env/
outputs/args/ATerm) and `SUI_DEBUG_CYCLE` (prints Blackhole re-entry thunk
identity + the full force stack). Both shipped, zero-cost when unset.

**The divergent drv field.** `pkgs.libxcrypt.drvPath` = `q9b9v7a9…` (sui) vs
`jb9k6090…` (nix). The final-stdenv libxcrypt is byte-identical to nix in every
field *except* `nativeBuildInputs`: nix has the perl dep (`jhz6cf…-perl` in env,
`az4wk58…-perl.drv` in inputDrvs); sui **drops it** — forcing `nativeBuildInputs`
raises `InfiniteRecursion`, which `derivation.rs`'s `Err(_) => continue` silently
swallows into a corrupt drv.

**Thunk-identity verdict: SAME thunk, false cycle.** `same_thunk_on_stack=true`,
`recursive_flag=false`. The re-entered thunk is the crypt+thread-disabled perl
(`pkgs/stdenv/linux/default.nix:354` — `perl = super.perl.override {
enableThreading = false; enableCrypt = false; }`). The cycle:
`perl540 (interpreter.nix, enableCrypt default true) → propagatedBuildInputs
demands libxcrypt → libxcrypt.nativeBuildInputs = [perl.override{enableCrypt=false}]
→ forcing that override re-enters the blackholed perl540/overlay thunk`. In nix
there is no cycle: the crypt-disabled perl never demands libxcrypt, and the base
perl's *result attrset* (carrying `.override`) is a resolved, memoized value by the
time deep deps force.

**Decisive isolation probes — all byte-match nix, proving the drv logic is
correct and the bug is purely fixpoint/blackhole demand-order:**

| Probe | sui result |
|---|---|
| `(derivation p.libxcrypt.drvAttrs).drvPath` | `jb9k6090…` ✅ |
| `(p.libxcrypt.override {}).drvPath` | `jb9k6090…` ✅ |
| `p.libxcrypt.drvAttrs.nativeBuildInputs` | `jhz6cf…perl` ✅ |
| `(builtins.head p.libxcrypt.nativeBuildInputs).drvPath` | `az4wk58…` ✅ |
| `p.libxcrypt.drvPath` (the real fixpoint demand) | `q9b9v7a9…` ❌ (nbi dropped) |

### RESOLVED (2026-07-10) — engine-level semantic fixpoint promotion, default-ON

`pkgs.libxcrypt.drvPath` now byte-matches nix (`jb9k6090…`) **by default, no env
gate**. The fix is the Blackhole↔Promise machinery the target section below
specified, applied at the **demand-order engine** instead of at syntactic
construction time (because the overlay `self:super:` self-reference threads through
`self`/`super`/`callPackage` across file boundaries, so `is_self_recursive_binding`
— a syntactic RHS name search — structurally cannot see it).

**What ships (default-ON):** when a thunk re-enters a `Blackhole` that is the SAME
thunk currently on the force stack (`force_stack_contains` — a genuine fixpoint
self-reference, not a distinct-thunk sharing gap), the engine retroactively
**promotes** that `Blackhole` to a real `ThunkRepr::Promise(cell)` and returns the
cell's in-progress partial — exactly what a `recursive`-at-construction thunk does,
including outer-body cell population (`is_promise || became_promise`) and the
`IN_PROMISE_EVAL` softening that makes `x.y or default` fall through like nix. This
satisfies `sui-spec/src/laziness.rs`'s `overlay-fixpoint` discipline
(`RecursionKind::Fixpoint ⇒ recursive + Promise`) in practice — the
`overlay_fixpoint_forces_to_a_promise_not_infinite_recursion` +
`every_authored_discipline_is_correctly_classified` locks are green.
(`sui-eval/src/value.rs`, the `ThunkRepr::Blackhole` arm + the outer-force
`became_promise` reconciliation.)

**Why it is NOT the blank sentinel that was rejected.** The earlier
`SUI_FIXPOINT_PARTIAL` gate returned a blank empty-attrs on re-entry, left the
thunk in `Blackhole` forever, and stack-overflow-**aborted** `(import <nixpkgs>
{}).hello.drvPath`. The promotion instead installs a first-class `Promise` cell
that the outer body populates and that transitions cleanly to `Evaluated`. The gate
has been **removed**.

**The two runaway backstops (release-active, armed only after a promotion fires).**
The promoted empty-attrs partial is byte-correct for the native-system fixpoint
(`libxcrypt`), but is the WRONG partial where a demand indexes it as a list /
non-attrs — the cross-system Darwin `apple-sdk` path `hello` hits under
`builtins.currentSystem = macOS` (via `elemAt (elemAt deps 1) 1` /
`makeOverridable`), which recurses without bound. Release disables the general
`MAX_EVAL_DEPTH` guard (`usize::MAX`) to admit nixpkgs' legitimately-deep
fixpoints, so nothing else stops that before the OS stack aborts. Two cheap
backstops, both gated on `promotion_occurred()` (untouched otherwise), catch it:
an **eval-depth** bound (`eval::PROMOTION_RUNAWAY_EVAL_DEPTH`, for runaways that
climb `eval_expr`) and a **force-stack-depth** bound
(`value::PROMOTION_RUNAWAY_FORCE_DEPTH`, for runaways that climb the force stack).
Either converts the would-be native abort into a recoverable `InfiniteRecursion`
that `x.y or default` recovers exactly like nix — so `hello` returns to a **clean
value-diverge**, not an abort. The converging `libxcrypt` fixpoint peaks well below
both bounds and is unaffected. `let r = r; in r` still errors (the promoted partial
can't progress → the depth backstop fires).

**Measured result.** Tail basket (`system = x86_64-linux`) went **10/23 → 11/23**:
`libxcrypt` flipped diverge→**MATCH**, every prior match stayed MATCH, every prior
diverge stayed a clean diverge — **0 regressions, 0 aborts**. Sealed corpus stays
**19 match · 2 tracked · 0 regressions · sealed**. `hello` remains a `KnownDiverge`
(its own root is an independent **fetchurl/mirrors** divergence, `pvir9l70…` vs
`gc56b6ig…-stdenv-linux`) — the promotion no longer touches its outcome negatively.

**The remaining terminal fix (deferred, larger).** The promoted partial is an
**empty** attrset. The truly-correct partial is the **materialized attrset with its
keys present** (the lazy attribute thunks), which nix has because the fixpoint
attrset value is materialized before its attribute thunks force. With that, the
`__structuredAttrs`/env-loop force error on cross-system dep positions would coerce
correctly rather than needing the best-effort skip in
`builtins/derivation.rs` — at which point that skip can be **surfaced** as a typed
error (surfacing it today regresses `libxcrypt` itself back to a `throw` and a dozen
clean diverges into hard errors — measured — so the skip deliberately stays until
the materialized-keys partial lands). Same architectural family as
`docs/M2.6-MODULE-SYSTEM-FIXPOINT.md`.

**Tail measurement (23-package basket, unchanged this session — no fix landed):**
base 10/23. `SUI_BLACKHOLE_AS_EMPTY_ATTRS` graduates only libxcrypt → 11/23; the
other 12 diverge on independent stdenv-stage / fetchurl roots, not this cycle. The
"~9-package cascade" earlier estimated for this root is **not confirmed** — the
diverging tail has multiple independent roots.

## The one-line law

A byte that differs between sui and nix is a **red gate**, forever — and the only
way to make it green is to make the bytes identical. The typescape is how that law
is stated once and enforced on every future variant without new engineering.
