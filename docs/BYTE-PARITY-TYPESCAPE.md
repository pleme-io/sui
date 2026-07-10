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

## The one-line law

A byte that differs between sui and nix is a **red gate**, forever — and the only
way to make it green is to make the bytes identical. The typescape is how that law
is stated once and enforced on every future variant without new engineering.
