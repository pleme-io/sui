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

**The fix DIRECTION is byte-verified — but a blank empty-attrs is NOT the final
fix.** Returning an empty attrset on same-thunk fixpoint re-entry (gated behind
`SUI_FIXPOINT_PARTIAL`, off by default) makes `pkgs.libxcrypt.drvPath` byte-match
nix (`jb9k6090…`) — the re-entered `__spliced.buildHost` access on an empty-attrs
partial falls through the `or drv` to the correct raw perl. It also keeps the whole
sealed corpus at **19 match / 0 regressions** and graduates the crypt-disabled-perl
tracked row. HOWEVER it is **NOT universally correct**: under the corpus's default
`<nixpkgs>` pin, `(import <nixpkgs> {}).hello.drvPath` turns from a wrong-value
diverge into a downstream **TYPE ERROR** (`sui-err`) — because a blank empty-attrs
is the wrong partial where the fixpoint's not-yet-complete value is a non-attrs or a
specific attr. So it is shipped **gated, off by default** (the default gate stays at
19/2/0 with no hello degradation); flipping it on is a measured degradation of the
hello row, not a clean win.

**The load-bearing fix (unchanged target).** Return the re-entered thunk's ACTUAL
in-progress value (the `Promise`-cell partial that `sui-spec/specs/laziness.lisp`'s
`recursive-binding` discipline specifies), NOT a blank sentinel — so the fixpoint
sees its own not-yet-complete real value. That requires the overlay/`fix`-fixpoint
attrset thunks to carry the `recursive` flag (a `Promise` cell), which the syntactic
`is_self_recursive_binding` detector cannot set because the self-reference threads
through `self`/`super`/`callPackage` across file boundaries. This is the same
rearchitecture as `docs/M2.6-MODULE-SYSTEM-FIXPOINT.md`.

**The precise remaining fix (the M2.6-sibling rearchitecture).** The `recursive`
classification is **syntactic** (`eval.rs::is_self_recursive_binding` — RHS names
its own binding), so it fires for literal `rec {…}`/`let` self-refs but MISSES the
nixpkgs **overlay / `self: super:` fixpoint** where the self-reference threads
through `self`/`super`/`callPackage` lambda args across file boundaries (the
`perl = super.perl.override {…}` overlay binding is a plain-attrset binding whose
RHS names `super.perl`, not `perl`, so it stays `non-recursive` → hard Blackhole).
The fix: make overlay/fix-fixpoint-participating attrset thunks classify as
`recursive-binding` (Promise re-entry returning the partial attrset), so genuine
fixpoint cycles return the in-progress value while `let r = r` still errors.
Same root as `docs/M2.6-MODULE-SYSTEM-FIXPOINT.md` (`extraArgs ↔ matchedOptions`)
— both are fixpoints nix tolerates via per-thunk lazy sharing that sui's syntactic
recursion detection can't see. Landing it graduates the two tracked corpus rows
(hello cascades separately — its own divergence is a **fetchurl-stdenv/mirrors**
root, `pvir9l70…` vs sui's `gc56b6ig…-stdenv-linux`, an independent target).

**Tail measurement (23-package basket, unchanged this session — no fix landed):**
base 10/23. `SUI_BLACKHOLE_AS_EMPTY_ATTRS` graduates only libxcrypt → 11/23; the
other 12 diverge on independent stdenv-stage / fetchurl roots, not this cycle. The
"~9-package cascade" earlier estimated for this root is **not confirmed** — the
diverging tail has multiple independent roots.

## The one-line law

A byte that differs between sui and nix is a **red gate**, forever — and the only
way to make it green is to make the bytes identical. The typescape is how that law
is stated once and enforced on every future variant without new engineering.
