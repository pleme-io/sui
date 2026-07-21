# SUI-EQUIVALENCE — sui ≡ nix in every way, faster and more beautiful

> The middle-altitude destination for making **sui a total, drop-in equivalent of
> nix** — byte-identical where nix defines the byte, provably so on every run,
> and *more legible* on every surface the operator touches. This doc is a
> **superset** of the one canonical parity destination: it inherits
> `SUI-SUPREMACY-ROADMAP.md` + the `CLAUDE.md:22-84` north-star **by reference**
> and adds the three axes the roadmap does not cover — **perf-honest-target,
> CLI-surface completeness, distribution/install** — plus a proof-architecture
> upgrade and an operator-visible spine. It is honest (§II) that it *also* mints a
> broader marquee and issues an execution plan that touches the parity spine, and
> it defers the parity-spine numbering to the roadmap rather than forking a rival.
> Tier labels are honest: **SHIPPED** (real today) / **DESIGN** (specced,
> buildable, n=0) / **UNMEASURED** (a number that does not yet exist). A
> `Result::Err` is *mitigation*; a compile error or absent method is
> *unrepresentability*; a green CI job is a *forcing-function*. Never round up —
> including where a measurement refuted a claim this fleet's own docs currently
> make (§IX).

---

## §I. The destination, stated unhedged

**sui IS nix.** The operator types `nix run .#rebuild`; on the fleet the
load-bearing eval and build calls resolve to sui and to the sui daemon on the
socket; darwin-rebuild's PATH no longer places a cppnix store path first; cppnix
is removed from the load-bearing rebuild closure and **retained only as the
differential oracle**. What sui produces is byte-identical to what cppnix
produces wherever nix defines the byte — the same store, the same hash/NAR/drv,
the same binary-cache wire, the same CLI contract, the same daemon protocol, the
same nix.conf semantics — and what nix *refuses* to produce (a legible,
progress-bearing, span-erroring, parity-showing terminal surface), sui produces
in addition. And the equivalence is **a theorem, not a machine-local claim**: a
full-closure ATerm differential from one instantiation, a reproducible-oracle CI
seal, an isolated-store NAR differential, daemon-protocol conformance, and a
tameshi-attested transferable receipt — enforced where it runs, with an honest
`Partial` always representable.

Three claims, as one statement:

- **Equivalent in every way** — every divergence from nix is a *defect*, driven
  to genuine 100% across seven differential surfaces (§III), each with a
  reproducible oracle and a mechanical gate. "In every way" is bounded to a
  finite, recorded, replayable object: **the exact call-trace a real
  `darwin-rebuild switch` makes**, replayed against sui, identical stdout + exit
  per call.
- **Faster** — as a consequence, honestly scoped and *sequenced last* (§IV): on
  allocation-bound hot shapes, on the fleet-shared warm-eval loop nix
  structurally lacks, and — the destination, gated behind bounded-memory eval —
  cold. No speed factor is *asserted* until sui completes the cid eval once and
  the number exists.
- **More beautiful** — the most defensible claim and the one already ahead of nix
  (§V): sui emits 100% typed escapes through kazari today (which sui **consumes**),
  and the destination extends that floor from rarely-run inspection views to the
  everyday build / eval / error path, adds live progress, semantic OSC marks, and
  a coloured parity matrix. Those additional surfaces render through fleet
  primitives that are **shipped-and-available but not yet consumed by sui**
  (egaku-term, mado) or **not yet a crate** (pente) — so on the everyday/progress/
  OSC/palette surfaces the beauty claim is a **promise (DESIGN — §V)**, not a fact
  today.

The load-bearing product is the **rebuild ribbon** (working label **`fita`** —
Brazilian-Portuguese *ribbon/tape*, a Tier-2 flow-family name, **unratified per
`NAMING.md`**): the one styled, progress-bearing, span-erroring, parity-showing
surface that streams back on every run. `fita` owns **no new algebra** — it
composes **kazari + egaku-term + mado's OSC channel + a live parity stamp**. (Its
palette projection through `pente` is **DESIGN, not a current input** — `pente` is
a theory doc today, not an available crate; see §V F6.) Equivalence-proof and perf
are scaffolding that hang off the ribbon and are *visible through it*, so the long
correctness work happens in the open.

That is the destination. It is not "sui, but nicer" — it is the deletion of
cppnix from the daily path, the collapse of three duplicate eval caches into one,
and the conversion of a 77-expression parity claim into a 20,827-node closure
theorem carrying a transferable receipt.

---

## §II. Relationship to `SUI-SUPREMACY-ROADMAP.md` — cite / extend / supersede

**There is exactly one canonical parity destination — the roadmap's — and this
doc does not replace it, but it is honest that it does more than "add three
axes."** The hierarchy already co-states one coherent destination:
`CLAUDE.md:22-84` (WHY / invariant), `SUI-SUPREMACY-ROADMAP.md` (the
dependency-ordered parity plan), `DARWIN-PARITY-CAMPAIGN.md` (the live last-mile
executor), `CONVERGENCE.md` (the forever-rebuild runtime companion),
`BYTE-PARITY-TYPESCAPE.md` (the gate typescape). This doc **inherits that parity
destination by reference** and adds three axes the roadmap does not cover
(perf-honest-target, CLI-surface completeness, distribution/install). **Three
honest caveats, stated up front so this doc does not masquerade as a pure
superset:**

- **It mints a broader marquee.** §I's "sui ≡ nix in every way, faster and more
  beautiful" is wider than the roadmap's "rebuilds cid byte-identical." That is a
  **deliberate superset banner over the roadmap's destination, not a rival
  destination** — the roadmap + `CLAUDE.md:22-84` remain THE parity authority. A
  reader who takes §I as *the* sui destination should read the roadmap first; the
  net-new ambition here is the three added axes, not a new parity claim.
- **Its milestone plan (§VII) spans the parity spine.** M2/M3/M6/M7/M9 touch
  eval-completion, real-rebuild shadowing, isolated-store realize, native daemon
  build, and the flip — the same ground as the roadmap's Phase A–D + Wave 1–3. To
  avoid two competing numbering authorities, **the parity-spine milestones defer
  to the roadmap's phase numbering as the authority** (cross-walk at §VII head);
  only the **new-axis increments** (the `fita` ribbon, CLI-golden-diff,
  perf-honest gating, distribution) are this doc's own. §VII is a *superset
  execution view*, not a second canonical plan.
- **It names a different marquee gate than the roadmap.** This doc holds **L0
  bounded-memory eval** as the live gate (§IV); the roadmap still names **M2.6
  module-system fixpoint** as the sole hard gate (`SUI-SUPREMACY-ROADMAP.md:42,113`).
  **M2.6 is CLOSED** (`docs/M2.6-MODULE-SYSTEM-FIXPOINT.md:3`), so the roadmap's
  gate claim is stale — but until the roadmap is updated, two live docs give two
  "what blocks the marquee" answers. The resolution is to **update the roadmap**
  (tracked as G1), not for this doc to quietly install a rival gate.

**CITE (do not restate):**

- `CLAUDE.md:22-84` — the north-star (nix→sui flip, same shared store,
  byte-identical hash/NAR/drv/binary-cache; "100%, byte for byte — table stakes").
  THE destination invariant; this doc points at it.
- ROADMAP §2 critical-path table — cited for the parity-axis structure.
  **Tier-vocabulary note (do not round up): this doc does NOT reuse the roadmap's
  `REAL / REAL-gated / PARTIAL / SPEC-ONLY / PLANNED` legend.** Its ledger (§VIII)
  uses BUILD.md's **SHIPPED / DESIGN** vocabulary plus a newly-minted
  **UNMEASURED** tier for the perf axis, and **MEASURED / REFUTED / CLOSED**
  status labels. That is a third tier vocabulary — named here so the divergence is
  explicit rather than claimed-as-verbatim-reuse.
- `DARWIN-PARITY-CAMPAIGN.md` §1–4 + M0–M5 — the doc closest to ground truth
  (newer than the ROADMAP, the actual live executor, carrying the measured
  aarch64-darwin banner and the M2.6-closed correction). **On the reading list.**
- `CONVERGENCE.md` (whole) as the runtime companion; `BYTE-PARITY-TYPESCAPE.md`
  as the gate typescape.

**EXTEND (the genuine, non-duplicative charter — three axes the roadmap explicitly
does not cover):**

- **Perf-honest-target.** The roadmap defers perf to `EVAL-CORE-DOMINANCE.md`,
  which sets an explicit *non-goal*: "~1.5–2× of nix, NOT match nix," and calls
  chasing nix's wall/mem a safety-thesis regression. A naive "faster in every
  way" contradicts an already-adopted sui doctrine; §IV reconciles it, it does
  not override it.
- **CLI-surface completeness.** The roadmap names only `nixos-rebuild-cli-shim`
  (gated). Per-flag / per-output / per-protocol contract completeness is not a
  roadmap axis (§III).
- **Distribution / install.** Not in the roadmap at all, and structurally blocked
  (the darwin-rebuild PATH problem, §IX C1).

**SUPERSEDE-WITH-BANNER (still-live sections that lie):**

- ROADMAP **§0, §2-row-3, §3-Phase-A, §5** are all predicated on "M2.6
  module-system fixpoint is the OPEN sole gate, being worked now." **M2.6 is
  CLOSED** (`docs/M2.6-MODULE-SYSTEM-FIXPOINT.md:3`); the marquee gate moved to the
  darwin/perf frontier (this doc: L0). `DARWIN-PARITY-CAMPAIGN` already corrects
  this by name, but the ROADMAP's own §0 still asserts the closed gate as open.
  This doc banners those sections as **superseded** and never cites them as live —
  the single largest stale claim in the destination hierarchy, and the root of the
  two-docs-two-gate-answers tension named above (G1).

**CARRY-A-CORRECTION wherever cited:**

- The **"C2 external-observation ceiling"** — that sui "cannot re-hash root-owned
  bytes" — appears as settled fact in **two** destination docs (ROADMAP §C0
  `:158-161`, `DARWIN-PARITY-CAMPAIGN` §1 `:39-45`) and was **refuted
  empirically**: sui hashed a root-owned store path byte-identically to nix. What
  survives is narrower — an input-addressed `out_path` equality *tautology*, and
  on a shared store two independently-built byte-sets cannot coexist at one
  content address to compare. Any citation of "C2 ceiling" must carry this
  correction or it enshrines a false "cannot."

**NEVER hardcode a coverage count.** The number drifts across every doc (ROADMAP
~38 · EVAL-CORE 64 · DARWIN 58→77 · `coverage.lisp` 22/10/7). Cite
`sui-spec/specs/nix_replacement_coverage.lisp` + the live `sui parity` tally,
never a fixed number.

---

## §III. What "equivalent in every way" means, enumerated

"In every way" is not one claim; it is **seven differential surfaces**, each with
a reproducible oracle and a mechanical gate. This decomposition *is* the
equivalence contract.

| # | Surface | Oracle | Gate | Today's tier |
|---|---------|--------|------|--------------|
| **S1** | Eval-algebra | env-independent expressions | `sui parity` row model (two-spawn, STDOUT byte-compare) | **SHIPPED, CI-enforced** (~40 rows) |
| **S2** | Instantiation / closure | pinned toplevel, full drv closure | one-instantiation graph-walk ATerm byte-diff | operator-only + name-level |
| **S3** | Realization / output bytes | isolated store, independent build | NAR byte-compare | 2-trivial-row scaffold (tautology on shared store) |
| **S4** | CLI contract (flags/JSON/exit) | recorded `nix …` golden capture | per-subcommand golden diff | name-level, contracts partial |
| **S5** | Config (nix.conf/NIX_PATH/features) | `nix show-config --json` | effective-config diff | **absent** (nix.conf unread) |
| **S6** | Daemon protocol (client+server) | recorded nix-daemon transcript | wire-protocol conformance replay | client-only, server can't build |
| **S7** | Distribution (drop-in on PATH) | a real `darwin-rebuild switch` | 100%-served, closure byte-equal | **PATH-blocked (C1)** |

Enumerated concretely, the surface the CLI/protocol axis must reach byte-for-byte:

- **Subcommands + flags.** `sui build` today binds `json: _, print_out_paths: _,
  no_link: _, dry_run: _, out_link: _, rebuild: _` (`src/main.rs:6086`) — it
  **discards every flag** and emits a bare path list that darwin-rebuild's
  `nix build --json | jq -r '.[0].outputs.out'` cannot consume (**B1**). The nix
  contract requires the full `--json` schema, `--out-link`/`--no-link`,
  `--print-out-paths`, `--dry-run`, `--rebuild`.
- **Global flags.** `--option`, `--max-jobs`, `--cores`, `--keep-going`, `-L`,
  `--impure`, `--log-format`, `--extra-experimental-features`,
  `--no-write-lock-file`, `--accept-flake-config` are accepted but **semantically
  ignored** (0 consumption sites) (**B5**). `--impure` and
  `--log-format internal-json` are the dangerous silent losses.
- **Output contracts.** `path-info --json` returns the store-path hash-part where
  nix returns `narHash`, hardcodes `references` empty, omits
  `narSize`/`deriver`/`signatures`/`ca`/`valid`, and is not an array (**B8**);
  `show-config` prints 3 lines and discards `--json` where nix emits ~100 settings
  (**B9**).
- **Exit-code contract.** `main` returns `Result<(), CliError>` with no
  `Termination`/`ExitCode` → every failure is exit 1 + `Error: {Debug}`
  (`main.rs:5632`), and the no-fallback shim adds a foreign exit 78 (**B3**).
- **Legacy binaries.** `nix-hash` → clap error; `nix-store --export/--import`
  absent; `nix-instantiate` without `--eval` force-mapped to `eval` (**B11**).
- **nix.conf + env.** Neither `/etc/nix/nix.conf` nor `~/.config/nix/nix.conf` is
  parsed at all; substitute-first is hardcoded in `daemon_realize.rs` (**B4** — the
  single largest silent-behaviour gap). `NIX_PATH` *is* honored on the eval path.
- **Flake refs.** The eval path resolves registry/indirect/`github:`/`flake:`/
  `git+https:`/`tarball:` refs via `sui-eval/src/builtins/flake_registry.rs`; the
  *realize* paths do not (`FlakeRef::parse` is filesystem-only,
  `sui-compat/src/flake_ref.rs:37`) (**B2**).
- **`nix run`** execs the app **without building it first** (`main.rs:6657`) —
  works only on a warm store (**B10**).
- **Daemon.** `BuildPaths` (op 9) + `BuildDerivation` (op 36) exist in the
  `WorkerOp` enum but fall through to "not yet implemented"
  (`sui-daemon/src/connection/dispatch.rs:50-70`) — today builds work only by
  **delegating to real cppnix** (**B6**); the client hardcodes protocol 1.37 and
  reads-and-discards the peer's advertised version, interoperating with cid's
  ~1.38 by backward accommodation only (**B7**).

### The proof-architecture switch (load-bearing)

Two proof models, chosen per surface:

- **Row model — keep, for S1 only.** The environment-independent algebra rows
  stay the `sui parity` two-process spawn model. It is correct and cheap at that
  tier, and genuinely CI-enforced today.
- **One-instantiation graph-walk — new, for S2/S3.** The two-process row model
  **cannot scale** to cid's ~20,827-drv closure (that is 20k evaluator spawns).
  The replacement is ~80% shipped: `bisect_drv` (`src/main.rs:4763`) already pairs
  `input_derivations` by name and recurses, but descends only the **first**
  diverging child. Generalize it to **visit-all + per-node ATerm byte-diff**,
  driven by ONE sui + ONE nix instantiation of a toplevel, then per-node NAR
  byte-diff on an isolated store (S3). This turns the seal from a 77-expression
  claim into a 20,827-node closure theorem from two evaluations, and **localizes
  any divergence to its exact root drv**.

### The two hard truths this design refuses to launder

- **The shared-store realize tautology.** On a shared multi-user store sui
  **never builds** (`realize_drv`'s Daemon arm only delegates), so "Realized" =
  input-addressed path validity = a *tautology*; two independent byte-sets cannot
  coexist at one content address to compare. **Independent-bytes equivalence
  REQUIRES the isolated store of S3** — there is no shortcut, and the refuted "C2
  ceiling" must never be re-enshrined as a reason to skip it.
- **CI enforces only the algebra tier.** `parity.yml` hardcodes
  `SUI_PARITY_PUREONLY=1`, so CI proves the ~40 environment-independent rows and
  skips the nixpkgs rows; the nixpkgs tally is an operator-machine (cid) result.
  Root cause: a floating `<nixpkgs>` oracle — a `DETERMINISTIC-INSTANTIATION`
  violation. **A seal is only real where its oracle is reproducible where
  enforced.** Until the pinned-oracle work lands (§VII M5), the nixpkgs tally is
  cited as machine-local, never as a CI-enforced theorem.

### Seal semantics — keep the instruments un-blinded

Every row/node carries `Expect ∈ {Match, KnownDiverge(reason)}`. An *unexpected*
divergence **or** a *silent graduation* (a `KnownDiverge` that started matching
without a row update) → exit 1. Three lying instruments were just fixed
(unconditional `SUI_PARITY_PUREONLY=1`, `coverage_at_100.rs`'s `==100%`, the
two-files-neither-implemented catalog invariant); the current tally is what
un-blinding revealed. **No gate in this campaign may make an honest `Partial`
structurally impossible or a nixpkgs-eval collapse un-greenable.** `KnownDiverge`
also makes *intentional* superiority a first-class non-red state — the mechanism
that lets "match nix exactly" and "be more beautiful than nix" coexist (a beauty
divergence on a typed-escape surface is a declared `KnownDiverge`, not a defect).

### The transferable artifact

The terminal proof is a **tameshi-attested receipt** `(oracle-pin + nix-version,
per-surface verdict vector, BLAKE3 chain)` — a third party runs `kensa verify`
and reproduces every surface verdict. The honest checkable claim is **"equivalent
to nix vX.Y at pin Z"**, not a floating "equivalent to nix". For S4/S6 the
mechanical oracle is a **recorded real-rebuild call-trace**: capture the exact
sequence of nix calls darwin-rebuild makes, replay each against sui, require
identical stdout + exit per call — which bounds "in every way" to a finite,
recorded, replayable object.

---

## §IV. Speed — the levers, MEASURED vs UNMEASURED, and the bifurcation

**The honest floor, stated first.** The marquee sui wall/mem **DOES NOT EXIST**:
sui has never completed `nix eval .#darwinConfigurations.cid.system.drvPath` (nix:
**107s**; one sui run was SIGKILLed at 4264s by an *unrelated* maintenance job
deleting its binary — not an OOM, not a sui bug). The **"3× on 45/48" headline**
(`CLAUDE.md:17`, `README.md:17`) is a **GHOST** — prose in two files, zero
harness, contradicted by the only real sui-vs-nix harness (`vs_nix_hotshapes.rs`:
**1.86× engine geomean, 9× LOSS on `rec_fib_20`** — the deep-recursion shape a
system-rebuild eval hammers). Every marquee factor is projection from the
nixosSystem proxy (~1982 baseModules: **wall 6.1×, mem 10.3×, instr 9.5×**).
Memory is ~22GB vs nix's 10.68GB (≈ 2× representation overhead, **no leak**). The
fast bytecode VM **cannot** produce the marquee (it bridges per-file to the
tree-walker on nixpkgs import and defers string-context, so it yields **wrong
drvPaths**); the parity-correct engine is the slow `--no-vm` tree-walker — the
engine that has never finished cid.

**The bifurcation — this design's sharpest honesty.** "Faster" splits into two
**disjoint** lever families, and the title's causal claim ("because pleme-io
substrate") is true for one and false for the other. The Pg/Redis/Tiered/
super-cache-ci substrate is **binary-cache only** and touches eval speed
**nowhere**; cold-eval pain is pure evaluator/interning throughput.

**§IV.a — Substrate levers (compounding, honestly "because substrate"):**

| Lever | File / harness | Status |
|---|---|---|
| **Fleet-shared warm eval (E1)** — migrate the eval-cache shared tier off on-disk redb `GraphStore` onto `Store`/`TieredBackend` | `eval_cache.rs`; `PgStore`/`RedisBackend`/`TieredBackend` (real, mock-parity-proven, config-select-wired, **unwired in any live eval path**) | local warm hit **== nix MEASURED** (the 407× whole-result cache); *fleet-shared* (machine B reuses A) **UNMEASURED** — a capability nix structurally lacks |
| **Build scheduling + paced fetch (E5/E3)** — shigoto Dag wave-schedule the realize graph; serve from the tiered store; samba-pace substituter RPCs | shigoto Dag; samba `LeakyBucket` | **UNMEASURED**, gated behind S3 isolated-realize |

**§IV.b — Evaluator levers (hard, ORTHOGONAL to substrate, where the marquee
actually rides):**

| Lever | File / dhat evidence | Status |
|---|---|---|
| **Bounded-memory eval (L0/D4/D6)** — columnar attrsets / env-repr, ~22GB→~17GB projected | two dhat profiles **disagree** (attrsets 45% vs env-bind HAMT-COW 42.7% / AST 40.7%) — needs ONE reconciling run | **the gate**: the cold cid eval OOMs before writing a cache entry, so no cache helps until this lands. **Projected, UNMEASURED which lever** |
| **rowan AST re-walk elimination (D3)** | dhat: 40.7% bytes / 51% alloc-calls / 21% wall self-time; up to ~20% wall | largest un-taken wall lever, **named-not-attempted, UNMEASURED** |
| **Deep-recursion closure** | `vs_nix_hotshapes.rs` `rec_fib_20` = 9× loss | must reach ≤ 1.0 or the marquee "faster" is impossible; **currently LOSING** |
| **Interning `ContentMemo` (E4)** | `sui-intern/src/memo.rs` | the only *existing* real cold-eval lever |

**The doctrinal fork, surfaced not overridden.** `EVAL-CORE-DOMINANCE.md` sets
"~1.5–2× of nix, NOT match nix" and calls wall-match chasing a safety-thesis
regression. A **cold** marquee eval has no warm cache and no build to schedule —
so the substrate levers do not apply, and "faster in every way" on that path
requires the raw evaluator to reach ≤ nix. Because the destination's word is
fixed, this design resolves the fork *toward* the destination (raise the
cold-eval goal, fund the D-cluster levers) but **may not assert any speed factor
until sui completes cid once and the number exists.** "Faster" is reconciled in
three honest senses, in the order they become true:

1. **Fast-FEELING (shippable now).** A legible progress surface (built on the
   shipped-and-available egaku-term v0.3.1, once wired into sui) makes a 107s eval
   *read* as motion — decoupled from wall-clock, the single biggest daily-
   experience delta, and it buys operator patience for the real perf work.
2. **Faster on allocation-bound hot shapes + fleet-warm reuse** (substrate-caused,
   §IV.a).
3. **Faster cold — the destination, gated behind L0, UNMEASURED.**

**Causal caveat, plainly:** the title's "because pleme-io substrate → faster" is
**TRUE** for warm-eval-sharing and beauty, **FALSE** for cold-eval speed. Delete
the "3× on 45/48" ghost or provenance-tag it (`CLAUDE.md:17`, `README.md:17`);
replace it with the measured number when it exists.

---

## §V. Beauty — what the operator sees, rendered by which fleet primitive

The most defensible dimension. sui is **already ahead of nix on typed escapes
today**: `grep -c x1b src/main.rs` = **0** (the only ANSI escape in the tree is a
test assertion at `style.rs:457`), and NO_COLOR + truecolor→256→16 degradation
ship — nix does none of this. That floor is real because sui **consumes kazari**
(in `sui-spec`, used in `sui-spec/src/style.rs`) and **tameshi** today. But
"because it is built on pleme-io substrate" is a **fact only for those two
consumed primitives.** The everyday / progress / OSC / palette renderers this
section's destination needs — **egaku-term** (v0.3.1, a real `Buffer::diff` — the
QUADRO M0 diff-render debt is CLOSED), **mado**'s OSC channel, and a **pente**
palette projection — are **not consumed by sui** (egaku-term + mado are
shipped-and-available primitives; `pente` is a theory doc, not yet a crate). So
beauty on the everyday/progress/OSC/palette surfaces is a **promise (DESIGN —
F3/F5/F6), not a fact.** The defect is **inversion**: styled on rarely-run
inspection views, raw on everything the operator actually runs — and the
everyday-path renderers are unwired.

| Surface | What the operator sees at the destination | Renderer | Gap |
|---|---|---|---|
| build / eval / store-copy / registry | styled, capability-degraded output on the **everyday** path (not just inspection views) | **kazari** (consumed) | F1 (raw at `main.rs:1156-1160`) |
| error path | a **styled source-span diagnostic** (`file:line:col` + underlined span), not `Error: <Debug>` (default Rust `Termination`, `main.rs:5632`) — the single most visible nix-vs-sui quality delta | **kazari** (consumed) | F2 (small effort, highest payoff) |
| a running build | a **live progress / spinner surface** (nix's most-criticised omission; today 0 spinner / `\r`) | **egaku-term v0.3.1** typed `Cell`/`Buffer` diff-render (**not yet consumed by sui**) | F3 (DESIGN) |
| any `/nix/store` path, any prompt | **clickable OSC-8 hyperlinks + OSC-133 prompt/command/output marks** — sui runs *inside* mado, which owns both ends of the terminal: the fleet's own-both-ends advantage nix structurally cannot use | **mado** (**not yet consumed by sui**) | F4 (DESIGN) |
| `sui parity` | an aligned, coloured **(probe × context) matrix**, not a single-char dot-stream (`parity.rs:58-89`) | **egaku-term** + **kazari** (egaku-term not yet consumed) | F5 (DESIGN) |
| the palette | sui's CLI palette projected through PENTE's existing `kazari` (CLI/activation) face — or a new surface *proposed* to PENTE's `(defroles)`/`(defface)` vocabulary — **deleting the full hand-typed Nord table at `sui-spec/src/style.rs:38-53` that PENTE's recon missed** (advancing PENTE's deletion-not-emission predicate) | **pente** (theory doc today) / **kazari** / **ishou** | F6 (DESIGN; `pente` has no crate yet) |
| in-repo | **TYPED-EMISSION** enforced by a `clippy.toml` `format!()`-ban matching kazari's own bar — a **broad, binary-wide refactor** (below) | (clippy gate) | F7 |

**The TYPED-EMISSION constraint.** Per the org-level rule, every operator-visible
string comes from a typed surface — `write!()` inside a `Display`/`Serialize`
impl, a typed logging/error macro, or a typed AST renderer. sui already clears the
**escape-sequence** bar in `main.rs` (`grep -c x1b` = 0). It does **not** yet
clear the **`format!()`** bar: this is a *separate axis*, and `main.rs` **alone
holds ~346 `format!()` sites** — all in production paths (`CliError` message
construction at `:821/:838/:868/:895/:911/:925/:942/:954/:999`, `bisect_drv` at
`:4774/:4776`, PATH/run construction at `:6650-:6662`) — with more in
`sui-daemon/dispatch.rs` and elsewhere. The `with-format-ban` clippy gate
(`disallowed_macros = ["std::format"]` + `-D warnings`) would fire on **all** of
them. F7 is therefore a **broad, binary-wide refactor** — nearly every `format!`
is a `CliError` string that TYPED-EMISSION wants routed through a typed
error/`Display` surface — **not** a "box-helpers" cleanup, and not an "application-
wiring exercise, not a build." The DESIGN grade is right; the size is not small.
The rendering substrate (kazari) is shipped, so wiring is available — but F7 is
real, binary-wide work, not a wave-through, and beauty on the everyday/progress/
OSC/palette surfaces is a promise for the primitives sui does not yet consume.

---

## §VI. The proof ladder — per-commit / per-day / per-release gates

Equivalence is proven by a **ladder** of gates at three cadences, whose apex is
the full-closure differential and whose seal is the tameshi receipt.

**Per-commit (fast, CI-enforced today for S1):**

- **S1 eval-algebra row model** — `sui parity` two-process spawn, STDOUT
  byte-compare over the ~40 environment-independent rows; red on any byte of
  divergence or on a silent graduation. This is the ratchet: a `Match` that
  regresses fails CI.
- **S4 CLI-golden-diff (as it lands, §VII M0+)** — per-subcommand: capture
  `nix <cmd>` on the **pinned** oracle, byte-compare `sui <cmd>` stdout + exit.
- **F7 format-ban** — `cargo clippy -- -D warnings` with `disallowed_macros =
  ["std::format"]` (a broad binary-wide refactor, per §V).

**Per-day (heavier oracle, operator-machine until reproducible):**

- **S2 closure differential** — the generalized `bisect_drv` (`src/main.rs:4763`)
  walks **every** input-derivation node of a pinned toplevel from **one**
  instantiation, per-node ATerm byte-diff; every non-match carries a
  `KnownDiverge` naming its root drv. Reports `nodes-byte-equal / total`.
- **nixpkgs eval track** — the live `sui parity` nixpkgs tally on cid.
  **Cited as operator-machine-only** until the pinned-oracle work (§VII M5) makes
  it CI-reproducible.

**Per-release (the theorem + the transferable artifact):**

- **S3 isolated-store NAR differential** — on an isolated single-user store where
  sui builds independently, NAR-compare a package set K; report
  `packages-NAR-byte-equal / packages-built`. The only path from "same graph" to
  "same bytes."
- **S6 daemon-protocol conformance** — replay a recorded nix-daemon transcript
  against `sui daemon`; version negotiation against cid's live daemon.
- **S7 distribution seal** — over N consecutive real `nix run .#rebuild` runs, the
  resulting system-closure toplevel drv byte-equals the cppnix-built closure;
  `nix-wrap-calls-served / total = 100%` (zero exit-78 fall-throughs).
- **A4 tameshi receipt** — `kensa verify` reproduces every per-surface verdict
  offline on an independent machine; the honest claim is "equivalent to nix vX.Y
  at pin Z".

**The full-closure differential is the apex.** It is what converts a 77-expression
claim into a 20,827-node closure theorem from two evaluations — and every gate
above it inherits the **oracle-discipline invariant**: a pinned-nixpkgs flake
input, a recorded nix-daemon transcript, a committed golden CLI capture — never a
floating `<nixpkgs>`, never an operator-machine-only result presented as CI-proven.

---

## §VII. Phased path — the milestones, every exit criterion a MEASUREMENT (or, for a rendered-surface artifact, a byte-diff against a committed golden)

> **Numbering authority (per §II).** The parity-spine milestones below
> (M2 eval-completion, M3 real-rebuild shadow, M6 isolated-store realize, M7 native
> daemon, M9 the flip) **defer to `SUI-SUPREMACY-ROADMAP.md`'s Phase A–D + Wave 1–3
> as the canonical numbering** — they are this doc's *execution view* of that
> spine, not a rival plan. Only the **new-axis increments** (M0 `fita`/CLI-golden-
> diff seed, M1 operator-verb ribbon, M4 CLI/config completeness, M5 CI-reproducible
> seal, M8 substrate-backed store) are this doc's own axes. Where a milestone below
> and a roadmap phase disagree, the **roadmap wins on the parity spine**; this doc
> wins only on its three added axes.

**M0 — CLI-golden-diff rail + `fita` seed + first row (dodges C1).** The operator
runs `nix eval <green-corpus-attr>` and `nix build --json .#<pinned-trivial>`
through a sui-first interactive alias; sui's output is byte-golden-diffed against
a live/pinned cppnix oracle, rendered through kazari (styled) + egaku-term
(progress) + a live parity stamp (`sui ≡ nix — byte-match`). Its load-bearing
defect fix is **B1** (`Commands::Build` discards every flag, `main.rs:6086`).
Tier-honest: **NOT** `.#darwinConfigurations.cid…drvPath` (sui has never completed
that) — M0 must be an eval sui completes and byte-matches **today**, cited from the
live `sui parity` tally, never a fixed count.

**M0 exit (a) is a COMBINED S4(CLI-shape) + S2(drv-instantiation) deliverable, not
merely "fix B1."** It becomes a runnable byte-diff only after **three** things land,
each named:

1. a real **`--json` emitter** for `sui build` (the B1 fix — absent today);
2. a **specific pinned-trivial flake-attr target** — *name and pin it in this
   criterion* — **confirmed drv-byte-green** (its drvPath + outputs already
   byte-match nix). **This surface has no precedent in the repo:** the only
   build-parity shipped today is `Commands::BuildParity` (`main.rs:6164-6200`), a
   hand-authored inline-`derivation{}` **expr-basket** compared via
   `nix hash path` — **not** a flake-attr and **not** `nix build --json`. So a
   drv-green *flake-attr* target must be selected and pinned before exit (a) can
   run; and
3. the **JSON field-set + field-order made byte-identical** to nix's `--json`.

*Exit (a):* with (1)–(3) in place,
`diff <(sui build --json .#<the-pinned-attr>) <(nix build --json .#<the-pinned-attr>)`
= 0 bytes.
*Exit (b):* `sui eval <green>` byte-identical to a live cppnix eval.
*Exit (c):* exit codes equal nix's, and the CI gate is red on any byte of
divergence.
*Exit (d) — NOT a measurement, a deliverable + captured-artifact inspection:*
`fita` renders under NO_COLOR + truecolor + 256 + 16, each captured as an
artifact. (To promote (d) to a measurement, byte-diff the rendered frame against a
**committed golden** per color mode.)
Live tally: **one S4 row, Match, CI-gated on the pinned oracle.**

**M1 — `fita` covers the operator's direct verbs, flag-honest + closure mechanism
on a small toplevel (S2).** Extend the ribbon to `nix build --json` / `nix eval
--json` / `nix flake …`; close **B3** (exit-code contract), **B5** (global-flag
consumption), **F2** (error span); add OSC-8/OSC-133 (**F4**); land
`with-format-ban` clippy.toml (**F7** — the broad binary-wide refactor, phased).
Generalize `bisect_drv` → full closure walk on a *small* pinned toplevel sui can
already complete. *Exit:* a golden-diff over the top-N daily verbs shows nix-shaped
stdout + matching exit on the green corpus; a forced error renders `file:line:col`
+ span; the closure differential visits every input-derivation node of the small
toplevel from ONE instantiation, each divergence localized to its root drv
(`nodes-byte-equal / total = 100%` OR every non-match carries a `KnownDiverge`).
**The cid-marquee closure differential (M1′) is named and gated behind M2/L0.**

**M2 — Bounded-memory eval: sui completes cid ONCE (the first real marquee
number).** Reconcile the two dhat profiles (**D4**); implement columnar-attrset /
env-repr (**L0**); follow with rowan re-walk (**D3**) + intern `ContentMemo`
(**E4**). *Exit:* sui completes `.#darwinConfigurations.cid.system.drvPath` under a
fixed memory ceiling (target ≤17GB, hard cap ≤22GB), records the **first** real
sui wall-clock vs nix's 107s, and the drvPath byte-matches cppnix. **This is the
first phase where "faster" is measurable on the marquee AND the cid rebuild path
becomes runnable on sui; no perf factor may be asserted before it.** Unblocks M1′.
(Parity-spine milestone — defers to the roadmap's Phase-A numbering.)

**M3 — Shadow the real rebuild (confront C1, zero regression).** Route
darwin-rebuild's internal instantiation to a sui SHADOW that byte-diffs against
cppnix while cppnix stays the executor (delegate-behind-the-shadow-gate).

**The interception mechanism, named — because M3's exit is load-bearing on it.**
With cppnix first on darwin-rebuild's PATH, a sui shadow **cannot** observe the
real rebuild's instantiation by any of the moves this campaign refuses (a PATH
reorder — the thing being avoided; an upstream `darwin-rebuild` patch; a global
nix-binary wrap). The **one candidate seam this doc controls** is an
**operator-owned, nix-darwin-provided instantiation wrapper**: *this very nix repo
owns darwin-rebuild's activation environment*, so nix-darwin can interpose a
wrapper that forks the internal `nix-instantiate` call to a sui shadow **without
touching the PATH cppnix resolves through**. If that operator-owned wrapper seam
turns out not to exist or not to be interposable (the residual bet, §IX Risk 1),
**M3's exit downgrades to conditional-on-seam-existence** and the marquee stalls at
the operator-verb tier until an upstream darwin-rebuild change lands — this doc
does not pretend the seam is guaranteed.
*Exit (conditional on the wrapper seam above):* over K consecutive real
`nix run .#rebuild` runs, sui's instantiation of the full cid system drv
byte-matches cppnix K/K; rebuild wall-clock statistically **unchanged**
(regression guard); a per-rebuild parity receipt is emitted; `fita` shows the
honest `eval:sui build:cppnix` badge. (Parity-spine milestone — defers to the
roadmap's real-rebuild-shadow numbering.)

**M4 — CLI/config contract completeness (S4 bulk + S5, recorded-trace-driven).**
Close **B4** (nix.conf parsing), **B8/B9** (path-info / show-config shapes),
**B10** (`nix run` builds-first), **B2** (registry/indirect flake-refs on
realize), **B11** (legacy `nix-*`) — only the subset the recorded darwin-rebuild
trace + the operator's verb set actually hit. *Exit:* the recorded call trace
replays call-for-call against sui with identical stdout + exit per call; nix.conf
substituters are honored (observable: sui fetches from the configured cache);
`--impure` / `--log-format` consumed not discarded; `sui show-config --json`
byte-delta vs `nix show-config --json` = 0.

**M5 — CI-reproducible ecosystem seal + attested receipt (A3/A4).** Pin the
`<nixpkgs>` oracle; make `SUI_PARITY_PUREONLY` **conditional** (the nixpkgs tier
becomes CI-enforced the moment the reproducible oracle lands); emit the tameshi
receipt. *Exit:* a pinned-oracle CI run (NOT just cid) reproduces the
operator-machine nixpkgs tally; `nixpkgs-rows-CI-enforced / total = 100%`; a
tameshi receipt verifies **offline** on an independent machine; an honest
`Partial` remains greenable-as-`Partial`.

**M6 — Isolated-store independent realize + NAR differential (S3).** Stand up an
isolated single-user store where sui builds independently; NAR-compare. *Exit:* on
the isolated store, `packages-NAR-byte-equal / packages-built = 100%` for a chosen
set K, K reported and growing per phase (the only path from "same graph" to "same
bytes"; resolves the shared-store tautology). (Parity-spine milestone — defers to
the roadmap.)

**M7 — Native daemon build + protocol conformance (S6).** Server-side
`BuildPaths`/`BuildDerivation` (**B6**, `dispatch.rs:50-70`); protocol negotiation
(**B7**). *Exit:* a client completes a build through `sui daemon` with **zero**
cppnix delegation; a conformance replay against a recorded nix-daemon transcript
passes; version negotiation succeeds against cid's live daemon. (Parity-spine
milestone — defers to the roadmap.)

**M8 — Substrate-backed durable store + fleet-shared warm eval (E1/E2/E3, D8).**
*Exit:* a warm eval on machine B **HITS** an entry written by machine A (a
capability nix lacks — the **first MEASURED strictly-better-than-nix result**);
`PgStore` NAR blobs byte-identical to on-disk `GraphStore` over a differential
corpus (graduating it LiveClusterProven); the three eval-cache impls
(`eval_cache.rs` 3-tier, `drv_cache.rs` redb, `sui-cache-eval` JSON) collapsed to
one.

**M9 — The flip (S7 / C1).** sui becomes the daily-path executor (eval + build)
with a shadow-gated cppnix fallback, then cppnix removed from the daily path; the
shadow-delegate deleted. *Exit:* a full `nix run .#rebuild` runs eval + build
through sui end-to-end and activates cid across N consecutive real rebuilds;
wall-clock recorded vs the cppnix baseline; `fita`'s parity strip green every run;
`nix-wrap-calls-served / total = 100%` (zero exit-78 fall-throughs); the resulting
system-closure toplevel drv byte-equals the cppnix-built closure (delta = 0); the
fallback is removable and rebuilds still succeed. (Parity-spine milestone — defers
to the roadmap's flip numbering.)

**Cross-cutting critical-path note.** M1′ (the closure differential on the actual
cid marquee) and any cid perf number **both** depend on M2/L0. The equivalence
spine (M1 on a smaller toplevel, M3–M7) advances **without** blocking on L0; only
the marquee claims on cid share L0's critical path — so "equivalent on cid" and
"faster on cid" are the **same gated milestone**, and the campaign keeps moving
while being honest about it.

---

## §VIII. Tier-honest ledger — destination vs shipped

Coverage is always the live `sui parity` tally + `nix_replacement_coverage.lisp`,
**never** a fixed count. Never round a tier up. **Tier vocabulary (per §II):**
BUILD.md's SHIPPED / DESIGN + a perf-axis UNMEASURED tier + MEASURED / REFUTED /
CLOSED status labels — **not** the roadmap's REAL/PARTIAL legend.

| # | Claim / surface | Tier | The honest gap |
|---|---|---|---|
| S1 | Eval-algebra byte-parity, CI-enforced | **SHIPPED** | ~40 env-independent rows; two-spawn model, red on divergence + on silent graduation |
| — | nixpkgs eval tally on cid | **SHIPPED (operator-machine only)** | `SUI_PARITY_PUREONLY=1` skips it in CI; floating `<nixpkgs>` oracle — cited as machine-local until M5 |
| S2 | Closure differential (full drv graph) | **DESIGN** | `bisect_drv` (`main.rs:4763`) is ~80% (descends first child only); generalize to visit-all |
| S3 | Realize / output-byte equality | **DESIGN** | 2-trivial-row scaffold; a **tautology** on the shared store (`realize_drv` Daemon arm only delegates); needs an isolated store |
| S4 | CLI flag/JSON/exit contract | **DESIGN (name-level SHIPPED)** | B1/B3/B5/B8/B9/B10/B11 open; `sui build` discards every flag (`main.rs:6086`); the only shipped build-parity is `Commands::BuildParity` (`main.rs:6164-6200`), an inline-`derivation{}` expr-basket via `nix hash path` — a different surface from `nix build --json` |
| S5 | nix.conf / effective-config parity | **DESIGN (absent)** | neither nix.conf read; substitute-first hardcoded in `daemon_realize.rs` |
| S6 | Daemon protocol (client + server) | **DESIGN (client-partial)** | `BuildPaths`/`BuildDerivation` "not yet implemented" (`dispatch.rs:50-70`); client hardcodes 1.37 |
| S7 | Distribution / drop-in | **DESIGN (PATH-blocked)** | darwin-rebuild PATH-hardcodes cppnix first; interception seam (M3) is an operator-owned nix-darwin wrapper — **candidate, not confirmed interposable** |
| Perf | Cold cid wall/mem factor | **UNMEASURED** | sui has never completed the cid eval; "3× on 45/48" is a ghost; real harness = 1.86× geomean, 9× loss on `rec_fib_20` |
| Perf | Warm local eval == nix | **MEASURED** | 407× whole-result cache hit; near-tautology (whole-result vs cold), cannot seed the marquee |
| Perf | Fleet-shared warm eval (machine B ← A) | **DESIGN (UNMEASURED)** | eval-shared tier on on-disk redb `GraphStore`, not `Store`/`TieredBackend`; a capability nix lacks once wired |
| Perf | Bounded-memory eval (L0) | **DESIGN (UNMEASURED which lever)** | two dhat profiles disagree; the cold cid eval OOMs before writing a cache entry |
| Beauty | Typed escapes (`main.rs`) — 0 raw `x1b` | **SHIPPED, ahead of nix** | escapes clean; but `format!()` is a **separate** axis — **~346 sites in `main.rs` alone** remain (plus `sui-daemon/dispatch.rs` etc.); F7 (the `with-format-ban` gate) is a **broad binary-wide refactor**, not a box-helper cleanup |
| Beauty | Styled everyday/error path + progress + OSC + parity matrix | **DESIGN** | inverted today — styled on inspection, raw on build/eval/error; renderers **unwired in sui** (only kazari + tameshi consumed; egaku-term + mado shipped-and-available, `pente` not yet a crate) |
| Substrate | `PgStore` / `RedisBackend` / `TieredBackend` | **SHIPPED (mock-parity), unwired in live path** | config-select-wired for CI; no live-cluster proof; binary-cache only, touches eval speed nowhere |
| Proof | tameshi-attested transferable receipt | **DESIGN** | A4; "equivalent to nix vX.Y at pin Z" is the honest claim |
| Doc | M2.6 module-system fixpoint | **CLOSED** | ROADMAP §0/§2-row-3/§3-Phase-A/§5 still assert it open — superseded (G1); this doc's live gate is L0, the roadmap still names M2.6 → two-docs-two-answers until the roadmap is updated |
| Doc | "C2 ceiling — sui can't re-hash root-owned bytes" | **REFUTED** | enshrined in ROADMAP §C0 + DARWIN-PARITY §1; survives only as the tautology + non-coexistence (G2) |

---

## §IX. Named tensions and open risks — including what today's measurements refuted

**Refuted by measurement (carry these corrections into anything cited):**

- **G1 — M2.6 is CLOSED**, not the open sole gate; ROADMAP §0/§2-row-3/§3-Phase-A/
  §5 are superseded. **This doc's live marquee gate is L0**; the roadmap still names
  M2.6 — a two-docs-two-answers tension resolved only by updating the roadmap, not
  by this doc installing a rival gate.
- **G2 — the "C2 ceiling" is refuted**; sui hashed a root-owned path
  byte-identically to nix. Only the input-addressed tautology + shared-store
  non-coexistence survive.
- **The "3× on 45/48" number is a ghost** — deleted or provenance-tagged; the real
  harness says 1.86× geomean with a 9× loss on deep recursion.

**Risk 1 — Equivalence-first commits to the two hardest, highest-uncertainty tiers
(S3 isolated-store realize, S7/C1 distribution), and until the flip lands NONE of
the proven S1–S7 surface is exercised in the REAL rebuild.** cppnix is still first
on darwin-rebuild's PATH and the no-fallback shim never receives the load-bearing
calls, so a mountain of green gates can coexist with "not yet a drop-in."
*Mitigation:* `fita` renders on the operator's **direct** verbs from M0 (visible
progress **without** C1); the shadow-delegate seam (M3) proves against the real
rebuild without regressing it; the campaign is only "done" at M9's real
`darwin-rebuild switch`. *Residual bet (load-bearing on M3's exit):* the
interception seam is an **operator-owned nix-darwin instantiation wrapper** this
nix repo controls — if darwin-rebuild's PATH construction is upstream and that
wrapper is **not** interposable, M3's exit downgrades to conditional-on-seam-
existence and the marquee stalls at the operator-verb tier until an upstream
darwin-rebuild change lands. *Sacrifice:* the "sui IS nix on cid" drop-in moment is
deferred to the end — correctly, but it means for M0–M8 sui is a proven-equivalent-
and-beautiful tool that is not yet the fleet's actual nix.

**Risk 2 — The FIXED "faster" collides with an adopted doctrine and with measured
loss, and cannot be claimed at all until L0 (large/xl, UNMEASURED) lets sui
complete cid.** `EVAL-CORE-DOMINANCE` calls matching-nix a safety regression;
`vs_nix_hotshapes` shows a 9× loss on the exact deep-recursion shape a rebuild
hammers; the marquee number does not exist; and the two dhat memory-attribution
profiles disagree (one reconciling run needed first). Holding "faster in every
way" fixed forces a doctrinal revision whose enabling levers (rowan-walk ~20%,
columnar attrsets 22→17GB, deep-recursion closure) are **all
projected-not-measured** — the single largest unquantified bet, concentrated in
M2. *Mitigation:* every perf claim is gated behind M2's completes-cid MEASUREMENT;
L0 is on the marquee-**equivalence** critical path too (it gates the cid closure
differential), so it cannot be indefinitely deferred; "faster" is honestly scoped
to the warm/shared loop (substrate-caused) + fast-FEELING (progress), reconciling
with — not overriding — the "~1.5–2×, not match nix" stance. The §IV bifurcation
quarantines the trap: no "faster because substrate" narrative on the cold path,
where it is false. *Sacrifice:* the safety-thesis doctrine that protects the
non-goal must be re-opened, and "faster" is the last dimension to become true.

**Risk 3 — Chasing 100% byte-equal CLI/output/config enshrines nix's mistakes,
over-fits ONE pinned nix version, and can suppress the very beauty advantage the
title wants.** Matching nix's quirky JSON, exit-78, exact nix.conf resolution is a
moving target across releases (the daemon already interoperates only by backward
accommodation to ~1.38); an oracle over-fit to cid's cppnix yields a proof true
against **one** nix; and forcing byte-equal **machine** output fights sui's
typed-emission lead (beauty *wants* divergence there). *Mitigation:* the
`KnownDiverge(reason)` seal arm makes intentional superiority a first-class
non-red state (beauty divergences declared, not defects); the oracle is a pinned,
reproducible input with its nix version recorded in the tameshi receipt, so
"equivalent to nix vX.Y at pin Z" is the honest, checkable claim. *Sacrifice:*
version-generality and sui's own better ideas on the few surfaces where "match nix
exactly" and "be more beautiful than nix" genuinely conflict — resolved by
**declaring**, not defaulting.

**Standing constraint (not a fourth risk) — don't re-blind the instruments.**
Three lying instruments were just fixed; the current tally is what un-blinding
revealed. Any new gate/ledger row keeps an honest `Partial` **structurally
representable** and a nixpkgs-eval collapse **un-greenable** — a seal is real only
where its oracle is reproducible where enforced (hence M5/A3 is its own tier, and
the nixpkgs result is cited as operator-machine-only until CI-reproducible).

---

## §X. Standing rule + waiver

**Standing rule.** Every sui-equivalence PR **advances a §VIII ledger-row tier or
leaves a typed `pending-sui-equiv: <row>` note** — and it **never lets the sealed
parity corpus regress** (a `Match` dropping to diverge, or a silent graduation,
fails the gate). Every parity fix obeys the **Parity Method** (`BUILD.md` §II.1):
solved once, for its whole class, in the typescape (`sui-spec` TYPED-SPEC triplet)
→ emanated through regeneration → sealed as a corpus row + catalog entry — never a
one-off keyed to a package, path, or instance; a genuinely instance-specific
divergence carries a `parity-oneoff: <reason>` note. Every perf claim is gated
behind a **completes-cid measurement** (§VII M2) and may not be asserted before
the number exists; every coverage figure cites
`sui-spec/specs/nix_replacement_coverage.lisp` + the live `sui parity` tally,
never a hardcoded count. This doc **inherits** `SUI-SUPREMACY-ROADMAP.md` +
`CLAUDE.md:22-84` by reference, defers the **parity-spine numbering to the
roadmap** as authority (§II, §VII head), and must not fork a competing marquee or
a rival gate. Time pressure is not an acceptable reason to ship an un-typescaped
patch, a rounded-up tier, or a re-blinded instrument.

**Waiver.** `skip-sui-equiv: <typed-reason>` at the top of a deviating context.
Acceptable: a genuinely non-nix-facing surface / a documented
`KnownDiverge(reason)` where "more beautiful than nix" is the deliberate,
declared choice on a typed-escape surface / pre-substrate-bootstrap. "The pipeline
was faster with cppnix," "we already know the number," and "time pressure" are
**not** acceptable reasons.

**The one sanctioned relaxation of the no-fallback constraint** is the
time-boxed, shadow-gated delegate-to-cppnix during M3–M8 shadow phases, so the
daily rebuild never regresses while equivalence is proven against it — removed at
the M9 flip.

---

**Files / anchors (all `pleme-io/sui`-repo-relative, from ground truth):**
`src/main.rs:{1156-1160,4763,5632,6086,6164-6200,6657,821,838,868,895,911,925,942,954,999,4774,4776,6650-6662}` ·
`style.rs:{38-53,457}` · `parity.rs:58-89` ·
`sui-eval/src/builtins/flake_registry.rs` · `sui-compat/src/flake_ref.rs:37` ·
`sui-daemon/src/connection/dispatch.rs:50-70` · `daemon_realize.rs` ·
`sui-intern/src/memo.rs` · `eval_cache.rs` / `drv_cache.rs` / `sui-cache-eval` ·
`sui-spec/specs/nix_replacement_coverage.lisp` ·
`docs/M2.6-MODULE-SYSTEM-FIXPOINT.md:3` · `parity.yml` · the perf harness
`vs_nix_hotshapes.rs`.

**Composition.** Extends `SUI-SUPREMACY-ROADMAP.md` + `CLAUDE.md:22-84` (the
inherited parity destination + numbering authority for the parity spine),
`DARWIN-PARITY-CAMPAIGN.md` (the live executor), `CONVERGENCE.md` (runtime
companion), `BYTE-PARITY-TYPESCAPE.md` (gate typescape), `BUILD.md` §II/§II.1
(sui as the stage-3 keystone + the Parity Method), `EVAL-CORE-DOMINANCE.md` (the
perf non-goal this doc reconciles), `PENTE.md` (the CLI-palette projection through
PENTE's existing `kazari` face — or a surface *proposed* to PENTE's `(defface)`
vocabulary; §V F6 — `pente` has no crate today), `SUPER-CACHE-CI.md` (the
Pg/Redis/Tiered substrate — binary-cache only), and `UNREPRESENTABILITY.md`
(determinism as a proof, not a convention). Grounds in CSE ("re-derive from the
typed source").

---

## Corrections applied in review

1. **§V + §VIII Beauty row (F7 size — OVERCLAIMED).** Removed the "residual
   `format!()` sites are the box helpers" / "application-wiring exercise, not a
   build" framing. Kept the verified-true `grep -c x1b = 0` claim, but stated
   `format!()` as a **separate axis** with **~346 sites in `main.rs` alone** (plus
   `sui-daemon/dispatch.rs`), nearly all `CliError` message construction, and
   re-described F7 as a **broad binary-wide refactor**, not a box-helper cleanup.
   The DESIGN grade is retained; only the rounded-down size is corrected. Anchors
   for the sampled sites added to the footer.

2. **§I + §V opening (sui "already consumes" 5 primitives — OVERCLAIMED).**
   Corrected to: sui consumes **only kazari + tameshi**; **egaku-term, mado are
   shipped-and-available but unwired in sui**, and **pente is a theory doc, not a
   crate**. Beauty on the everyday/progress/OSC/palette surfaces is re-labeled a
   **promise (DESIGN — F3/F5/F6)**, not a fact. The §VIII styled-path ledger row
   was updated to say the renderers are unwired in sui.

3. **§VII M0 exit (a) (reframed as combined S4+S2, not "fix B1" — OVERCLAIMED).**
   Rewrote M0 to state exit (a) is a **combined S4(CLI-shape) + S2(drv-
   instantiation)** deliverable requiring (1) the `--json` emitter, (2) a **named,
   pinned-trivial flake-attr target confirmed drv-byte-green**, and (3) a
   byte-identical JSON schema. Named the fact that the only shipped build-parity is
   `Commands::BuildParity` (`main.rs:6164-6200`), an inline-`derivation{}`
   expr-basket via `nix hash path` — a **different surface** from `nix build
   --json`, with no flake-attr precedent — and added it to the §VIII S4 gap column.

4. **§VII M0 exit (d) + §I fita composition (OVERCLAIMED).** Relabeled exit (d)
   from a "measurement" to a **deliverable + captured-artifact inspection**, with
   an explicit path to promote it to a byte-diff against a **committed golden** per
   color mode; softened the §VII header claim accordingly. In §I, corrected `fita`
   to compose **kazari + egaku-term + mado + a live parity stamp**, with the
   **pente** palette projection marked DESIGN (theory doc, not a current input).

5. **§VII M3 + §IX Risk 1 (C1 interception mechanism — OVERCLAIMED at the
   milestone exit).** Kept the honest risk framing but **named the interception
   mechanism**: an **operator-owned, nix-darwin-provided instantiation wrapper**
   this nix repo controls (not a PATH reorder, not an upstream patch, not a global
   nix-binary wrap). Made M3's exit **conditional on that wrapper seam existing/
   being interposable**, with an explicit downgrade + stall path if it does not,
   cross-referenced to Risk 1 and the §VIII S7 row.

6. **§II "one destination / adds only three axes" (OVERCLAIMED).** Replaced the
   false disclaimer with three honest caveats: (a) the doc **mints a broader
   marquee** (flagged as a superset banner over — not a replacement of — the
   roadmap's destination); (b) its §VII plan **spans the parity spine**, so the
   parity-spine milestones **defer to the roadmap's phase numbering** (added a
   "Numbering authority" note at the head of §VII and per-milestone deferral tags);
   (c) it **names a different marquee gate (L0)** than the roadmap (M2.6), resolved
   by updating the roadmap (G1), not by installing a rival gate. Did not fully
   renumber to P-A/X0 (out of scope for a correction pass), but made the numbering
   authority and the two-docs-two-gate tension explicit.

7. **§II tier-legend "reused verbatim" (FALSE).** Corrected the CITE bullet to
   state plainly that this doc does **NOT** reuse the roadmap's
   REAL/REAL-gated/PARTIAL/SPEC-ONLY/PLANNED legend — its ledger uses **BUILD.md's
   SHIPPED/DESIGN + a newly-minted UNMEASURED tier + MEASURED/REFUTED/CLOSED
   status labels**, a third vocabulary named as such. Added the same note to the
   §VIII ledger header.

8. **§V F6 + Composition footer ("22nd-duplicate Nord palette" / `(defface
   :surface cli)` — OVERCLAIMED).** Kept the confirmed substance (the real
   duplicate at `sui-spec/src/style.rs:38-53`), **dropped the "22nd" number**
   (ungrounded and colliding with PENTE's reserved-repo usage), reframed deletion
   as advancing **PENTE's deletion-not-emission predicate**, and replaced the
   invented `:surface cli` with PENTE's **existing `kazari` (CLI/activation) face**
   — or a surface *proposed* to PENTE's `(defroles)`/`(defface)` vocabulary. Marked
   the row DESIGN with "`pente` has no crate yet," and added `style.rs:38-53` to the
   anchors.