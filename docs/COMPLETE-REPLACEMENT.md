# COMPLETE-REPLACEMENT — what "sui replaces CppNix" MEANS, as a predicate

> **Status: DEFINITION + PLAN (2026-08-11).** No rung below is green for any node.
> This doc defines the predicate and the seals; it claims no progress against them.
> Every measurement cited was taken on 2026-08-11 unless dated otherwise.

## §I. Why a definition is the first deliverable

"sui replaces nix" has been a direction, not a predicate — so it could not be
finished, only felt. Worse, the instruments that appeared to measure it were
measuring something else, and each one failed in the direction that reads as
success:

| instrument | what it reported | what it measured |
|---|---|---|
| `sui parity` | 77/77, 0 regressions | CI sets `SUI_PARITY_PUREONLY=1` unconditionally (`parity.yml:74`). Same binary, same host, flag off: **41 match / 35 regressions / exit 1** (`src/main.rs:4594-4602`). Total loss of nixpkgs eval shipped GREEN |
| `sui parity` (engine) | sui ≡ nix | `--no-vm` (`main.rs:5045`) while `sui eval` defaults to the bytecode VM — the engine *proven* and the engine that *runs* are different programs |
| `sui build-parity` | output NAR parity | on a multi-user store the **nix daemon performed both builds** — the row is a tautology |
| `cli_parity`, `build_parity`, `full_build`, `real_nix_client` | `ok` | all four no-op without `SUI_TEST_ONLINE`, which **no workflow sets** |
| `kataFlipParity` (nix repo) | ceu "fails parity" | `tryEval`-wrapped: an eval **failure** is indistinguishable from a byte **divergence**. ceu never failed parity; it failed to evaluate |
| roadmap `store-add-path` | "CLI writes to ~/.cache" | stale — the CLI tries the daemon first (`main.rs:1699-1755`) |

The lesson is the seal's own: **an instrument that reports green while measuring
nothing is worse than no instrument.** So the definition must be a witness that
cannot be constructed without a measurement, and the measurement must carry its
own denominator.

## §II. The predicate

A node is **`Replaced`** iff a witness exists. The witness is not assertable —
it has no public fields, no `Default`, and no constructor except one that
consumes proof of every conjunct:

```rust
/// Witness that ONE node converges with no CppNix present.
/// Private fields + no Default: `Replaced` cannot be written down, only earned.
pub struct Replaced {
    node: NodeName,
    conformance: ConformanceWitness,
    ceiling: Ceiling,
}

impl Replaced {
    pub fn certify(
        node: NodeName,
        conformance: ConformanceWitness, // §III — all rungs green, denominator > 0
        absence: NixAbsent,              // no cppnix binary, no cppnix daemon socket
        steady: SteadyState,             // >= 10 consecutive converged reconciler ticks
        rollback: RollbackProven,        // one deliberate rollback, verified
        gc: GcSafe,                      // a full GC deleted nothing nix considers live
        ceiling: Ceiling,                // REQUIRED: the named residual. No `None` arm.
    ) -> Self { /* … */ }
}
```

`Ceiling` is a required argument, not an `Option`. Per the seal's tier-honesty
rule, a seal whose residual is unnamed is not a seal; making it non-optional is
how "we forgot to state what this still doesn't prove" becomes unrepresentable.

**Fleet-complete** is then `[Replaced; 18]` — one witness per configuration
(16 nixos + 2 darwin, measured `nix eval .#nixosConfigurations|.#darwinConfigurations`
→ 16 + 2). Not a majority, not "the live ones": the *denominator is the flake's own
output set*, so a config added later lowers the score until it too is witnessed.

### §II.1 The anti-vacuity constructor

`ConformanceWitness` is where the 77/77 lie is made unconstructible:

```rust
impl ConformanceWitness {
    /// `Err` when ANY rung measured zero comparisons — a suite that ran nothing
    /// cannot witness anything. This is the positive-only `Delta` seal applied to
    /// coverage instead of to speed.
    pub fn measured(rungs: [RungResult; 8]) -> Result<Self, Vacuous> {
        for r in &rungs {
            if r.compared == 0 { return Err(Vacuous::NothingCompared(r.rung)); }
            if r.engine != Engine::Shipping { return Err(Vacuous::WrongEngine(r.rung)); }
            if r.reclassified > 0 { return Err(Vacuous::Reclassified(r.rung)); }
        }
        // …
    }
}
```

Three refusals, one per lie in §I: **zero comparisons**, **a non-shipping engine**,
**any reclassified skip**. `PUREONLY` cannot be expressed through this constructor.

## §III. The rung ladder

Each rung is a differential against nix as oracle. Every rung states what it
compares and what it does NOT.

| # | rung | artifact compared | status 2026-08-11 |
|---|---|---|---|
| R0 | scalar on a real fleet expression | `kataFleetGate` = 29, both engines | **GREEN** |
| R1 | leaf nixpkgs `drvPath` | 10/10 byte-identical (2026-08-09) | **GREEN** |
| R2 | **node toplevel `drvPath`** | `minimal`: nix `szx145ay…` vs sui `aibwin419…` | **RED — measured** |
| R3 | `.drv` **ATerm bytes** | — | **ABSENT** (sui must instantiate, not just eval) |
| R4 | **closure set-difference** over the input-derivation graph | — | **ABSENT** (`parity-bisect` descends only the first child) |
| R5 | **output NAR hash**, per derivation, nixpkgs corpus | 2 synthetic rows only | **ABSENT for real packages** |
| R6 | **store state**: refs, deriver, sigs, `ultimate`, file modes | — | **ABSENT** |
| R7 | **nix as CLIENT against sui's socket** | — | **ABSENT — the keystone** |
| R8 | **activation text + system closure** | — | **ABSENT** |
| R9 | steady state: 10 ticks, rollback, safe GC | — | **ABSENT** |

**R7 is the keystone.** When `nix build`, `nix copy` and `nix-collect-garbage`
drive sui's socket successfully, "sui replaces nix" stops being our opinion —
**nix itself certifies it**, and the 36 unimplemented opcodes become a red list
that shortens instead of a design intention.

## §IV. The seals — each measured defect, cornered

The seal doctrine's two central patterns land directly on what we measured.

### §IV.1 The registry-parity seal → sui's daemon opcodes

`sui-daemon/src/connection/dispatch.rs:49-72` is an exhaustive `match` over the
opcodes sui knows. **An exhaustive match over your own enum seals only what is
already IN the enum** — it cannot see the ~36 nix opcodes sui never enumerated,
and the omission is silent *in the direction that reads as fine*: fewer variants
looks like a smaller supported surface, never like a bug. This is textbook
registry-parity: sui's enum mirrors an external registry (nix's worker protocol).

```rust
const NIX_PROTO: &str = include_str!("…/nix/src/libstore/worker-protocol.hh");
let declared = scan_wopcodes(NIX_PROTO);       // scan EVERY declaration form
let covered  = SuiOp::ALL.map(wire_tag);
assert!(declared.len() >= NIX_OPCODE_FLOOR,    // else a broken scan reads as green
        "the scan itself broke — this gate would be vacuously green");
assert!(declared.difference(&covered).is_empty()); // unimplemented: the real defect
assert!(covered.difference(&declared).is_empty()); // phantom: fails against nix
```

**TIER: CI-caught (C2).** Cross-language set-equality is not expressible in Rust's
types. **Destination:** generate `SuiOp` from the protocol header so two lists
collapse into one and the seal moves to truly-unrepresentable. Per
UNREPRESENTABILITY: **record one red run** (delete a variant; the gate must name it)
before landing.

### §IV.2 Option-plus-predicate → four fallible constructors

Each of these is `Option<T>` + an ignorable predicate today. The seal is the same
move every time: non-optional field, private, fallibility in a constructor at the
boundary where the observation happens.

| defect (measured) | today | the seal | tier after |
|---|---|---|---|
| Substitution files content at the **wrong address** and reports success (`substitute.rs:178-181` → `add_to_store` recomputes the path) | `add_to_store(name, …)` cannot honour a demanded path | `add_to_store_at(demanded: StorePath, …) -> Result<_, AddressMismatch>` — the demanded path is an **argument**, so filing elsewhere has no expressible path | **truly-unrep** |
| `try_substitute_outputs` returns `true` without re-checking validity (`local_builder.rs:478-496`) | `bool` | return `Valid` — a witness obtainable **only** from `is_valid_path` | **truly-unrep** |
| `register_path` **silently drops** refs whose target isn't registered (`local.rs:298-311`, `if let Some` with no `else`) | refs are best-effort | `ResolvedRefs::observed(&[StorePath]) -> Result<_, UnregisteredRef>` | **truly-unrep** |
| Sandboxes **fail open** to `NoSandbox` and return success (`sandbox.rs:317-325`, `:480-489`) | success is a `bool` | registration requires a `Sandboxed` witness; `NoSandbox` **cannot produce one** | **truly-unrep** |

**The tell that each is done right:** a test you can no longer write. When
`assert!(result.is_valid())` becomes impossible to fail, the seal moved from
only-mitigated to truly-unrepresentable. If the assertion is still meaningful, it
did not.

### §IV.3 Only-mitigated, with ceilings named

| invariant | tier | ceiling |
|---|---|---|
| sui's store state ≡ nix's after a build | **only-mitigated (C2)** | external-world observation — the oracle is another process's SQLite |
| GC deletes nothing live | **only-mitigated (C2)** | runtime roots are `/proc` state, not a type. Today sui reads neither `temproots` nor runtime roots (`local.rs:728-747`) — strictly worse than the ceiling |
| Node activation ≡ nix's | **only-mitigated (C2)** | activation is non-transactional I/O against a live machine |
| drvPath equality | **CI-caught (C2)** | nix remains the soundness authority; a corpus proves only what it covers |

## §V. The plan, ordered by what it seals

**Phase 0 — make the instruments incapable of lying.** Drop `PUREONLY`; move the
corpus to the shipping VM engine; set `SUI_TEST_ONLINE` in CI; split
`kataFlipParity`'s `tryEval` into distinct eval-failure and divergence verdicts;
land `ConformanceWitness::measured` with its three refusals. *Exit:* every parity
number in the repo is reproducible on the engine that ships, with a stated
denominator, and a red run recorded per refusal.

**Phase 1 — R2 green.** The frontier is already located and red on `minimal`
(seconds to run, a real system). Build R3 and R4 first so a divergence localises
to a derivation instead of a hash. *Exit:* byte-identical toplevel drvPath, all 18
configs, shipping engine, no `--impure`.

**Phase 2 — the four §IV.2 constructors.** Each is small, each removes a class,
each is verified by a test becoming unwritable. *Exit:* all four at truly-unrep.

**Phase 3 — R7, the keystone.** Land the §IV.1 opcode parity gate first: it turns
"the daemon is incomplete" into an enumerated, shrinking list. Then implement
against that list until nix drives sui's socket. *Exit:* `nix build`, `nix copy`,
`nix-collect-garbage` succeed against sui.

**Phase 4 — R5, R6, R8.** Store state, real-package NAR parity (single-user store
or the row is void), activation bytes. NixOS additionally requires
`switch-to-configuration` and a bootloader entry — absent today (`system.rs:508`),
and the reason a NixOS `switch` half-converges and `boot` boots the old generation.

**Phase 5 — R9 per node, then absence.** Order by blast radius: **ryn** (physical
access, cid is a peer rebuilder) → **cid** → **zek** → **ceu** once provisioned →
**rio last** (only x86_64 builder + live sui cache origin + no console). `nix.enable
= false` is the **consequence** of `Replaced::certify` returning, never a decision.

## §V.1 R2 BISECTED (2026-08-11) — the frontier is `pkgs`, not size

The R2 divergence was bisected on `minimal`, the smallest config in the repo,
comparing `nix eval --raw` against `sui eval --no-eval-cache --raw` per attribute.

| attribute | nix | sui |
|---|---|---|
| `config.system.name` | `minimal` | `minimal` — **MATCH** |
| `config.nixpkgs.system` | `aarch64-linux` | `aarch64-linux` — **MATCH** |
| `options.system.name.type.name` | `str` | `str` — **MATCH** |
| `config.system.path.drvPath` | `jzddq8y2…-system-path.drv` | `i2xn2r8i…` — **DIVERGE** |
| `config.system.build.etc.drvPath` | `ng0ak1bc…-etc.drv` | `xwvwll4s…` — **DIVERGE** |
| `pkgs.hello.drvPath` | `lagmxdbc…-hello-2.12.2.drv` | **stack overflow** |
| `pkgs.hello.name` | `hello-2.12.2` | **stack overflow** |
| **`pkgs.system`** | `aarch64-linux` | **stack overflow** |
| `config.environment.systemPackages` | (list) | **stack overflow** |

**The minimal reproducer is one plain string on the smallest config:**

```
$ sui eval --no-eval-cache --raw '.#nixosConfigurations.minimal.pkgs.system'
fatal runtime error: stack overflow, aborting
$ nix eval --raw '.#nixosConfigurations.minimal.pkgs.system'
aarch64-linux
```

Three things follow, and they reframe the whole eval problem:

1. **It is not size.** This is the *smallest* config in the flake and a
   *string-valued* attribute — no derivation, no closure, no nixpkgs traversal
   the answer depends on. The cid OOM has been read for a month as "the fleet's
   configs are too big for a GC-less evaluator" (`EVAL-MEMORY.md`); a plain
   string on `minimal` overflowing says the reachable-set size is not the
   mechanism, or at least not the only one.
2. **It is not derivation hashing.** The module system itself works — plain
   options and the `options.*` tree evaluate and MATCH. The divergence and the
   overflow both live on the path to the **package set**.
3. **The two failures are related but distinct.** `config.system.path.drvPath`
   *evaluates* (and diverges), while `pkgs.system` *overflows* — so there are at
   least two defects on the pkgs path, and fixing the overflow will not by itself
   make R2 green.

**The overflow is INFINITE, not deep — measured 2026-08-11.** The recursion was
sampled mid-flight (`sample <pid>`), showing a tight repeating cycle thousands of
frames deep:

```
eval_expr → eval_expr_inner → eval_apply → apply_inner → apply
          → force_thunk → Thunk::force → force_inner → eval_expr → …
```

(4201 `eval_expr` frames in a 2-second sample.) The stack was then raised from
256 MB to **2 GB** on both eval threads and rebuilt: it still overflows. A debug
frame is on the order of a kilobyte, so 2 GB admits something like a million
frames — exhausting that is a cycle, not a depth.

**It is the VM engine, which is what ships.** The abort names its thread:
`thread 'sui-vm-eval' has overflowed its stack`. `sui eval` desugars
`flake-ref#attr` to `(builtins.getFlake …).attr` and runs it on the bytecode VM
(`src/main.rs:6204`), NOT the tree-walker (`eval_render_threaded`,
`src/main.rs:7859`, thread `sui-eval`). This is the same split the corpus has:
`sui parity` proves `--no-vm` while the shipped path is the VM.

**Consequence for the memory work.** `EVAL-MEMORY.md`'s entire frame — retention,
GC-less evaluator, columnar attrsets, peak RSS — is aimed at a size problem. A
plain string on the smallest config, looping forever inside 2 GB of stack, is not
a size problem. The memory work may still be worth doing; it is not what stands
between sui and a node eval.

**Note `EvalError::RecursionLimit` already exists** (`sui-eval/src/value.rs:3022`,
constructed at `:5079`) — so a depth guard is available and the VM path is not
using it. Converting this abort into a typed error is the smallest honest
improvement: SIGABRT kills the test process (which is why `nix_repo_cid_drv_path`
can never report), while a `RecursionLimit` is catchable, printable, and lets an
instrument record the failure instead of dying with it.

**What this changes about the plan.** Phase 1 previously read "close the drvPath
divergence, weeks, unknown shape". It now has a seconds-long deterministic
reproducer that needs no fleet config, no network, and no memory ceiling — the
single most tractable form this bug has ever had. The approved remedy in
`EVAL-MEMORY.md` (columnar attrset) **never landed** — `git grep "Rc<[Symbol]>"`
across every branch returns nothing; what shipped instead was HAMT → hashbrown
(`fab4566`, self-reporting -33% peak RSS on a *probe*, not on a node). So the
memory work is both unstarted and, on this evidence, aimed at the wrong target.

**Method note, recorded because it nearly cost the finding.** The first version
of this bisect compared two empty strings and printed `MATCH` — the probe's own
vacuity bug, the same shape as `PUREONLY` and `system_eval_parity`'s silent
skips. An empty or `error`-prefixed reading is now `UNMEASURED`, never a match.
A differential harness that cannot distinguish "equal" from "measured nothing"
will manufacture green at exactly the moment it matters.

## §V.2 R2 BISECTED TO ONE PACKAGE — and the VM/tree-walker split (2026-08-11)

**The tree-walker is a WORKING REFERENCE. The infinite recursion is VM-only.**

```
$ sui        eval …'#nixosConfigurations.minimal.pkgs.system'  → stack overflow (thread sui-vm-eval)
$ sui --no-vm eval …'#nixosConfigurations.minimal.pkgs.system'  → aarch64-linux   ✅ matches nix
```

The `--no-vm` run even emits nixpkgs' own deprecation warning (`'system' has been
renamed to/replaced by 'stdenv.hostPlatform.system'`), proving it is genuinely
executing nixpkgs rather than short-circuiting. So sui contains a second engine
that survives the fixpoint the shipped engine cannot. **This is the single most
useful fact for the eval work: there is an in-repo reference implementation to
diff the VM against, and the corpus's `--no-vm` choice — long recorded as a
weakness — is measuring the engine that actually works.**

**On the tree-walker, R2 bisects to exactly ONE package out of 189.**

| level | result |
|---|---|
| `pkgs.hello.drvPath`, `pkgs.coreutils.drvPath` | **MATCH** byte-for-byte — nixpkgs instantiation is correct |
| `config.environment.systemPackages` length (119), `pathsToLink`, `system.path.name` | **MATCH** |
| `system.path.drvAttrs` — every key except one | **MATCH** |
| `system.path.drvAttrs.pkgs` — 189 paths, 188 identical | **1 DIVERGES** |

The one: **`nixos-firewall-tool`** — nix `zcr0fpmb…`, sui `5qk60n31…`. Its own
`drvPath` differs (`bjvx9wnp…` vs `byc82max…`) while **every attribute in its
`drvAttrs` compares equal**, including `src`, once the source is materialised.

**A transient double-prefix was observed and then could not be reproduced.** The
first cold dump showed sui's `src` as
`/nix/store/<newhash>-<oldhash>-nixos-firewall-tool` — a store path re-added to
the store. Re-queried warm, both engines return `ar6s9jl9…-nixos-firewall-tool`.
So sui's `filterSource` over an already-in-store path is correct in isolation and
correct when warm; the double-prefix appears to be a **cold-path materialisation
artifact**, and it is NOT the residual divergence (which survives when `src`
matches).

**What the divergence is NOT** — each ruled out by a passing synthetic probe:
`toString null` / `typeOf null` / `toJSON` of a null field (all MATCH);
`__ignoreNulls` with a null attribute (MATCH); `__ignoreNulls` +
`__structuredAttrs` + explicit `outputs` together (MATCH).

**What is known about the target.** nix's real `.drv` for this package carries
**31 env keys and `userHook` is ABSENT** — `__ignoreNulls = 1` dropped it. The
derivation also sets `__structuredAttrs = true`. sui's equivalent `.drv` **cannot
be inspected: it was never written to disk** — sui computes a drvPath without
instantiating (the R3 gap, §III), which is precisely why this had to be chased
through `drvAttrs` instead of by diffing two files. **That is the tooling gap
this bisect proves the cost of.**

**Next probe, stated so it is not re-derived:** the divergence survives equal
`drvAttrs`, so it lives in derivation *construction* — the ordering, filtering or
serialisation applied between `drvAttrs` and the ATerm — reached via `stdenv`'s
`mkDerivation` rather than via a bare `derivation` call. R3 (write the `.drv`)
would answer it in one diff.

## §V.3 R3 BUILT; the firewall divergence is NOT reachable from the Nix level

**R3 shipped** (`SUI_EMIT_DRV=<dir>`, commit a4a0225): sui now writes the exact
ATerm bytes whose hash IS the drvPath, at all three construction sites
(`sui-eval/builtins/derivation.rs`, `sui-ir/derivation.rs`,
`sui-bytecode/vm.rs`). Opt-in; eval stays pure; a write failure is ignored.

**Finding while landing it: derivation construction is TRIPLICATED.** Three
engines each carry their own near-identical copy. Instrumenting one emitted 459
drvs while the derivation under investigation never appeared — which reads as a
caching artifact and is not. Three copies of derivation construction is three
places for parity to drift; retiring them into one helper is a PRIME DIRECTIVE
item, deliberately not done inside a diagnostic commit.

**The `nixos-firewall-tool` divergence resisted every probe available from the
Nix level.** Eliminated, each by measurement, not reasoning:

| hypothesis | how it was ruled out |
|---|---|
| `filterSource` double-prefixing an in-store path | matches in isolation AND warm; the double-prefix seen once on a cold path never reproduced |
| `null` / `toString` / `toJSON` handling | all MATCH |
| `__ignoreNulls` + `__structuredAttrs` interaction | synthetic combination MATCHes |
| sui's eval cache | cold `XDG_CACHE_HOME`: identical answer |
| silently-dropped attributes | `SUI_PARITY_STRICT` → *"no swallowed force-error drops (clean eval)"* |
| the redb drv cache (`drv_cache.rs`) | `~/.cache/sui/drv-cache.redb` does not exist; no hit/miss log on this path |
| a stale value rather than a computed one | `overrideAttrs (o: { pname = "probe-marker"; })` CHANGES sui's path — so it is genuinely computed |

**The contradiction that remains, stated plainly.** sui returns
`byc82max…-nixos-firewall-tool.drv`, responds correctly to an override, and yet
that derivation is never emitted by any of the three `compute_drv_path_with_refs`
sites in the same evaluation (455–459 OTHER drvs are emitted in the same run,
written seconds before the check, so the emitter demonstrably works). Either a
fourth construction path exists that does not use that helper, or the emission is
suppressed on this route. **Not yet explained, and not guessed at here.**

**Next probe, and it needs the inside of the evaluator, not another Nix-level
experiment:** put a panic-on-name or a backtrace at the three emit sites keyed to
`nixos-firewall-tool`, and see which stack actually produces the string. Every
external avenue is now exhausted; continuing to probe from Nix would be
re-deriving what these seven rows already settled.

## §V.4 The divergence is INVISIBLE from Nix — every observable agrees

Continued from §V.3 with an inside view. Three engines were instrumented and a
name-keyed backtrace added; the pre-existing `SUI_IR_DUMP_DRV` dumper on the IR
path was used as well (it had existed all along — my `SUI_EMIT_DRV` partly
duplicates it, which is itself the triplication problem showing up a second
time).

**Measured, and it is a contradiction worth stating flatly:**

- With the filter unset, `sui-eval`'s derivation builtin fires **547 times** in
  this evaluation, emitting 455 files. **`nixos-firewall-tool` is not among
  them.** Every one of the 547 is a fetch/bootstrap derivation — tarballs and
  `.cabal` files — with no build derivations at all.
- `SUI_IR_DUMP_DRV=firewall` on the IR path: **never fires.**
- The VM site: **never fires** (and the VM cannot reach this expression anyway,
  §V.1).
- Yet `overrideAttrs (o: { pname = "probe-marker"; })` **changes** sui's answer,
  so the path is genuinely computed, not replayed.

**And every Nix-level observable AGREES with nix:**

| observable | result |
|---|---|
| every key of `drvAttrs`, `src` included | MATCH |
| `buildInputs` drvPaths | MATCH (`f7zq8yia…-bash-interactive-5.3p3.drv`) |
| `builtins.getContext` of `src` | MATCH (`ar6s9jl9…-nixos-firewall-tool`) |
| the resulting `drvPath` | **DIFFERS** (`bjvx9wnp…` vs `byc82max…`) |

nix's own `.drv` records **5 `inputDrvs` and 3 `inputSrcs`**; sui writes no
`.drv` for this derivation at all, so the two cannot be diffed even with R3.

**Conclusion, stated as a limit rather than a guess.** Identical attributes,
identical inputs, identical string context, different hash — and no construction
event on any instrumented path. The divergence lives somewhere that neither the
Nix level nor the three derivation-construction sites can observe. Every external
avenue is exhausted; **this now needs a debugger on a running eval**, not another
experiment, and that is where it is left. Nothing here is guessed at.

**Method cost worth recording:** ~15 probes to establish that a divergence is
unreachable from outside. That is not a failure of the probes — it is the
strongest possible argument for the rungs the ladder already names (R3 writing a
`.drv` for EVERY derivation, R4 diffing closure sets), because those turn this
class of question into one `diff`.

## §V.5 STANDALONE REPRODUCER — 33 identical attributes, different hash

The divergence reproduces with **no flake, no module system, no fleet config** —
one import and one attribute:

```
E='(import /nix/store/4k79ns9…-source { system="aarch64-linux"; }).nixos-firewall-tool.drvPath'
nix       eval --raw --impure --expr "$E"  → /nix/store/bjvx9wnp…-nixos-firewall-tool.drv
sui --no-vm eval --raw --impure --expr "$E" → /nix/store/byc82max…-nixos-firewall-tool.drv
```

**With a clean control in the same nixpkgs:** `hello.drvPath` **MATCHES**,
`nixos-firewall-tool.drvPath` **DIVERGES**. Same import, same system, same
engine — so this is a property of the package, not of the evaluator's setup.

**nix's answer is config-independent.** Standalone and inside the fleet config it
returns the same `bjvx9wnp…`, so sui's value is the outlier in both settings and
the fleet config is not implicated at all.

**And every attribute is identical.** Comparing `drvAttrs` key-by-key with values
rendered through `toString`:

```
keys equal: True | nix 33 sui 33
VALUE DIFFS: NONE — every attribute identical
```

Also verified equal: `buildInputs` drvPaths, `builtins.getContext` of `src`, and
`src` itself under both the reconstructed predicate and the package's real
`lib.hasSuffix ".nix"` one.

**Therefore the divergence is in DERIVATION SERIALIZATION, not in evaluation.**
33 attributes that compare equal cannot produce two hashes unless the bytes built
from them differ — the ATerm's field ordering, escaping, output-set encoding, or
the `inputDrvs`/`inputSrcs` composition (nix's own `.drv` records 5 and 3
respectively). That is a much narrower and more tractable target than "the
evaluator diverges", and it is squarely inside `sui-compat/src/derivation.rs`.

**The blocker on closing it is still R3's coverage.** sui emits 455 ATerms in
this very run and this derivation is not among them, on any of the three
construction sites, while `overrideAttrs` proves the path is genuinely computed.
Something constructs it outside all three instrumented sites. **Finding that
site is now the whole task** — and it is a code-reading task in
`sui-eval`/`sui-compat`, not another differential experiment.

## §V.6 Serialization is byte-correct; the fault is upstream of it

**sui's ATerm is BYTE-IDENTICAL to nix's** for a derivation it gets right —
proven with R3 on `hello`'s tarball drv: `cmp` against `/nix/store/<same-name>`
succeeds. So `Derivation::serialize` is not generically broken, and the
serialization hypothesis of §V.5 is **narrowed, not confirmed**: whatever differs
for `nixos-firewall-tool` is not a systematic ATerm defect.

**The distinguishing feature of the failing package, found:** its `buildInputs`
select a **non-default output**. nix's own `.drv` records

```
bash-interactive-5.3p3.drv   outputs=['dev']     ← the only non-'out' input
ShellCheck / install-shell-files / stdenv-linux / bash   outputs=['out']
```

`hello` (the control that MATCHES) has no such input. This is the one structural
difference between them, and it was invisible to the earlier `buildInputs`
comparison because that compared **drvPaths**, which are identical regardless of
which output is referenced.

**But sui records the selection correctly.** `builtins.getContext` on that
buildInput returns, on both engines, byte for byte:

```
{"/nix/store/f7zq8yia…-bash-interactive-5.3p3.drv":{"outputs":["dev"]}}
```

So: identical attributes (33/33), identical inputs, identical output selection,
identical context, byte-correct serializer — and a different hash. The fault is
in how the multi-output selection is carried from context into the
`input_derivations` map, or in the modulo resolution of that entry — after the
values are correct and before the bytes are written.

**One promising code path, not yet proven:** `serialize_modulo`
(`sui-compat/src/derivation.rs:246-274`) resolves each input to its modulo hash
and re-sorts — a fix whose own comment says it "survived until a 2-input stdenv
drv". The outputs *within* an entry are emitted in stored order and never sorted
(`:271`). With single-output inputs that is unobservable; `dev` is precisely the
case that could expose an ordering or selection defect. **Stated as the next
hypothesis, NOT as a finding — it has not been tested.**

**Ruled out this round:** a `pname` override renames the drv on neither engine
(both keep `nixos-firewall-tool`, correct — `name` is set explicitly in
`package.nix`), so there is no second naming bug.

## §V.7 The dev-output hypothesis is FALSIFIED; four more features cleared

§V.6 named the non-default (`dev`) output as the next hypothesis. **Tested and
wrong** — the minimal case matches exactly:

```
stdenv.mkDerivation { name="multiout-probe"; buildInputs=[ p.bashInteractive.dev ]; }
nix → 9ld89r8c…-multiout-probe.drv   sui → 9ld89r8c…  MATCH
```

Working forward from the real `package.nix` instead, every distinctive feature
was reproduced in isolation and every one **matches**:

| feature of the failing package | probe | result |
|---|---|---|
| non-default `dev` output input | `buildInputs=[ bashInteractive.dev ]` | MATCH |
| `stdenvNoCC` rather than `stdenv` | `stdenvNoCC.mkDerivation` + dev input | MATCH |
| `strictDeps` + native/build split | `strictDeps=true`, both input lists | MATCH |
| the `doCheck` bootstrap predicate | `buildPackages.shellcheck-minimal.compiler.bootstrapAvailable` → `1` both | MATCH |
| the `nativeBuildInputs` member | `installShellFiles.drvPath` | MATCH |
| `filterSource` src (§V.4) | both predicates | MATCH |
| all 33 `drvAttrs` (§V.5) | key-by-key | MATCH |

**`doCheck = false` still diverges** (`wbbqv4pc…` vs `6p26l54j…`), so
check-inputs are not the trigger either. Every ingredient is individually
correct; only the assembled derivation differs.

**And the decisive negative:** that `doCheck=false` override forces a FRESH
derivation with a hash neither engine had computed before — it cannot be replayed
from anything. sui emitted **71 ATerms in that run and this was still not among
them.** A derivation that is provably computed fresh, and provably not emitted at
any of the three instrumented `compute_drv_path_with_refs` sites, establishes a
**fourth construction path** as fact rather than inference.

**That is now the single blocking question for R2**, and it is a code-reading
task: find the code that produces a `.drv` store path for an `stdenv.mkDerivation`
result without calling `compute_drv_path_with_refs`. Until it is found, no probe
from the Nix level can see the bytes, because the bytes are never written.

## §V.8 FIRST PARITY BUG FIXED — and R2's residual is one list position

**FIXED (`56ac876`): a store path copied INTO the store kept its old hash.**
Found by diffing the ATerms once R3's emitter was moved to the real funnel:

```
nix src: /nix/store/ar6s9jl9…-nixos-firewall-tool
sui src: /nix/store/nr1fj1g7…-ar6s9jl9…-nixos-firewall-tool
```

sui derived the store NAME from the materialized basename, which for an in-store
path is already `<hash>-<name>` — so `nar_hash_source_tree` produced
`<newhash>-<oldhash>-<name>`. CppNix passes the name as a separate argument to
`addToStore` and never re-derives it, hence no divergence there. One shared
`strip_store_hash_prefix` in `sui-compat` now serves both call sites (not copied
— the triplication mistake §V.3 flagged), with 4 tests including a nix-base32
lookalike and a multibyte non-panic guard.

`nixos-firewall-tool` now matches nix **byte-for-byte** (`bjvx9wnp…`), and the
node toplevel moved `aibwin419…` → `adjbgkwa…` — the fix propagates.

**The probe that had been lying, and for how long.** The first R3 emitter sat in
`compute_derivation_outputs`, which not every derivation routes through: 455
ATerms emitted while the one under investigation never appeared. That was read as
a caching artifact twice and written up once as a "fourth construction path"
(§V.7). It was a **misplaced probe**. `compute_full_drv` is the single funnel;
once the emitter moved there the byte diff was immediate. **An instrument's
placement is part of its correctness** — this cost most of a session and is the
same class as every vacuity defect this doc catalogues.

**R2's residual: ONE list position.** `system.path.drvAttrs.pkgs` holds the same
189 paths with identical priorities; **8 positions differ, and all 8 are one
element moving**. `environment.systemPackages` has 37 definitions in both engines,
same files, same contents — but `banken.nix` is definition **#1** for nix and
**#8** for sui, before vs after the seven `tools.nix` definitions.

**Reproduced three ways, all MATCHING — so the ordering rule itself is correct:**

| synthetic probe | nix | sui |
|---|---|---|
| `mkIf` vs plain definitions | `plain1,plain2,FROM_MKIF` | same |
| module via `imports=` vs direct | `LAST_IMPORTED,b,a` | same |
| path-valued modules (the fleet's shape) | `z,b,a` | same |

Note all three agree that a LATER module's definition can come FIRST — the same
inversion nix shows for `banken`, which is appended last in
`lib/nodes.nix:203`. So sui implements the rule; something specific to the real
config's module graph selects a different position, and the synthetics do not yet
capture it. **That is the whole remaining gap for R2 on `minimal`.**

## §V.9 R2's residual REPRODUCED in two modules — nix re-orders, sui does not

The ordering divergence now has a minimal reproducer with no fleet machinery:

```nix
f.inputs.nixpkgs.lib.nixosSystem {
  system = "aarch64-linux";
  modules = [ <repo>/nodes/minimal <repo>/modules/shared/banken.nix ];
}
```
```
nix: banken.nix, tools×7     ← banken FIRST
sui: tools×7, banken.nix     ← banken LAST
```

**And it is NOT list order.** Swapping the two entries changes nothing on either
engine — nix yields `banken.nix` first both ways, sui yields `tools.nix` first
both ways. So nix is applying an ordering of its own to the merged definition
list, and sui is preserving declaration order (`banken` is appended last at
`lib/nodes.nix:203` and lands last).

**It is not lexicographic either**, which was the obvious next guess and is
wrong: `rbcgq4db…-source/modules/shared/banken.nix` sorts AFTER
`4k79ns9d…-source/nixos/modules/installer/tools/tools.nix`, yet nix places it
first. So nix's order comes from its module-graph traversal, not from a sort on
`_file`.

**Six synthetics reproduce the shape and all MATCH**, which is why this took so
long to corner — the rule is implemented, the input to it differs:

| probe | verdict |
|---|---|
| `mkIf` vs plain definitions | MATCH |
| module via `imports=` vs direct | MATCH |
| path-valued modules | MATCH |
| one module imported by two parents (dedup) | MATCH |
| `_file` pointing at another store input | MATCH |
| `nixosSystem` with two real modules | **DIVERGE** ← the reproducer |

**Found on the way, a separate bug:** given `modules = [ ./nodes/minimal … ]`
inside a `getFlake` expression, sui resolved the RELATIVE path against nixpkgs'
`lib/` directory rather than the flake root —
`import /nix/store/4k79ns9d…-source/lib/nodes/…` — and errored. nix resolves it
against the file that wrote it. Worked around with absolute paths for the
reproducer above; **logged here as its own defect**, not folded into the ordering
one.

**This is the whole remaining gap for R2 on `minimal`.** Every other input —
189 store paths, identical priorities, 33 drvAttrs, all inputs and contexts —
matches. One definition's position in one list.

## §V.10 MINIMAL REPRODUCER — four lines, one module, no fleet code

R2's residual reduces to this, and nothing in it is pleme-io's:

```nix
# /tmp/probe-empty.nix
{ config.environment.systemPackages = [ ]; }
```
```nix
nixosSystem { system = "aarch64-linux"; modules = [ /tmp/probe-empty.nix ]; }
```
```
nix: definition #1 is probe-empty.nix   ← the USER's module comes first
sui: definition #1 is tools.nix         ← a nixpkgs-internal module comes first
```

**`banken` was never special.** Any external module that touches
`environment.systemPackages` reproduces it; the fleet's `banken.nix` was simply
the first one in the list. The whole earlier investigation into what made that
package different was chasing a coincidence.

**nix's rule, read from its source rather than guessed.** `lib/modules.nix`
sorts definitions with `sortProperties` (`:1373`) by `mkOrder` priority, and
`lib.sort` is STABLE, so equal-priority definitions keep collection order. Since
`banken.nix` carries no `mkOrder` (grep: zero hits), priority is not the
mechanism — the two engines COLLECT definitions in different orders, and nix's
collection puts the caller's modules ahead of the imports pulled in beneath them.

**Ruled out along the way, each measured:**

| hypothesis | verdict |
|---|---|
| list order of `modules = [ … ]` | swapping changes NEITHER engine |
| lexicographic sort on `_file` | banken's store path sorts AFTER nixpkgs', yet nix puts it first |
| `builtins.sort` instability | stable on both (`LOW,A,B,C`) |
| `mkOrder` / `mkBefore` priority | absent from `banken.nix` |
| import-flattening depth (deep child vs later leaf) | MATCH |
| the module's own content | a 4-line stand-in reproduces it identically |

**Seven synthetics matched before this one diverged** — mkIf, imports=,
path-valued modules, dedup across parents, cross-input `_file`, flattening depth,
sort stability. The rule is implemented; the definition-collection ORDER differs,
and it only shows up through `nixosSystem`, which is why every hand-built
`evalModules` probe agreed.

**This is now a bounded fix in sui's module-system collection**, with a
four-line regression test available. It is the last known blocker for R2 on
`minimal`.

## §V.11 A third bug: `unsafeGetAttrPos` across a function boundary

Found while tracing what `nixosSystem` adds over bare `evalModules`
(`nixos/lib/eval-config.nix:28` derives `modulesLocation` from it):

```nix
# /tmp/pos.nix
args: builtins.toString ((builtins.unsafeGetAttrPos "modules" args).file or "NULL")

(import /tmp/pos.nix) { modules = [1]; }
  nix → NULL            # the attr came from the CALLER's expression
  sui → /tmp/pos.nix    # the CALLEE's own file
```

sui attributes the position to the file performing the lookup rather than the
file that wrote the attribute. Inline (`let a = { modules = [1]; }; in …`) both
return `NULL`, so this only appears across a function boundary — the exact shape
`eval-config.nix` uses.

**Not the R2 ordering cause** — measured: the `_file` recorded for the user's
module is `/tmp/probe-empty.nix` on BOTH engines, so `modulesLocation` lands
correctly despite the divergence. Logged as its own defect (the third found this
session) rather than being folded into the ordering hunt.

**Also ruled out as the ordering cause**, each measured: `concatMap`,
`attrNames`, `attrValues`, `catAttrs` (all MATCH — so the ordering-sensitive
builtins nixpkgs' `collectStructuredModules` relies on are correct), and
`builtins.sort` stability (§V.10).

**Where R2 stands.** sui runs nixpkgs' own `lib/modules.nix` — there is no Rust
reimplementation of module collection to fix — so the divergence must come from
a builtin that collection depends on, and the obvious candidates are now
eliminated. The next probe is to instrument `collectStructuredModules` from the
Nix side (wrap it and print the collection order on both engines), which is
cheap and would name the responsible builtin directly rather than by guessing.

## §V.12 The ordering divergence was an artifact of the MEASUREMENT HARNESS

§V.11 logged `unsafeGetAttrPos` as "not the R2 ordering cause" on the strength of
one check (`_file` matched on both engines). That check was sound but did not
reach the mechanism. Continuing the hunt found the mechanism, and it inverts the
conclusion — **for the `--expr` reproducer specifically**.

**The chain.** `nixos/lib/eval-config.nix:28` derives `modulesLocation` from
`(builtins.unsafeGetAttrPos "modules" evalConfigArgs).file or null`. When that is
non-null, eval-config wraps EVERY user module:

```nix
locatedModules = map (lib.setDefaultModuleLocation modulesLocation) modules;
#   setDefaultModuleLocation file m  =>  { _file = file; imports = [ m ]; }
```

The inner module keeps its own `_file` — which is why the `_file` check matched
and why it was the wrong probe. What changes is DEPTH: `collectModules` walks
with `builtins.genericClosure`, which is breadth-first, so the wrapper demotes
each user module one level and moves its definitions after every level-0 one.
This is the `{ imports = … }` trap the nix repo's own CLAUDE.md already records,
arrived at from the opposite direction.

**Measured on nix alone (the oracle), same expression, wrap toggled:**

```
unwrapped (modulesLocation == null):  kid3,kid2,kid1,USER,plain
wrapped   (modulesLocation != null):  USER,kid3,kid2,kid1,plain
```

nix's real output put the user module FIRST (the wrapped shape); sui's put it
after the seven `tools.nix` children (the unwrapped shape). So the two engines
disagreed about `modulesLocation`, i.e. about `unsafeGetAttrPos`.

**And that disagreement only exists when the caller has no source file:**

| caller of the attrset literal | nix | sui | |
|---|---|---|---|
| written in a `.nix` FILE | `/tmp/caller.nix` | `/tmp/caller.nix` | MATCH |
| written in `--expr` (no file) | `NULL` | the CALLEE's file | **DIVERGE** |

Every R2 ordering probe reconstructed the `nixosSystem` call in `--expr`. The
fleet never does: `nixosConfigurations.minimal` is `parts/nixos.nix:47`, a file.
**The reproducer manufactured the divergence it reported.**

**The lesson, which is the durable part: measure the fleet's own call shape, not
a synthetic reconstruction of it.** An `--expr` rebuild of a file-based call is
not the same expression — it differs in a property (having a source file) that
nixpkgs reads and branches on. Two sessions of ordering work were spent inside
that gap.

### The underlying sui defect (real, contained, NOT fleet-affecting)

`EVAL_FILE_STACK` is a `Vec<PathBuf>` (`sui-eval/src/eval.rs:40`) and therefore
cannot represent "evaluating something with no file". A thunk created in a
fileless context restores its file with `.map(push_eval_file)` (`:3273`) —
`Option::map`, so `None` pushes NOTHING and the callee's file stays on top.
`attach_attrset_positions` then stamps the literal with the callee's file.

Fix shape: make the stack `Vec<Option<PathBuf>>` and push an explicit fileless
frame. Deliberately NOT applied in the same change as an R2 measurement — it
alters relative-path resolution for fileless closures, and rebuilding the binary
mid-measurement would invalidate the run.

### Ruled out, each measured on both engines, each MATCH

`builtins.genericClosure` traversal order · `builtins.sort` stability at 42 equal
keys (the earlier stability check was under Rust's ~20-element insertion-sort
threshold, so it was re-run at the real size) · `concatMap` / `attrNames` /
`attrValues` / `catAttrs` · relative-path resolution across a lazy-import
boundary AND across a closure called from another file · `unsafeGetAttrPos` with
a file-based caller · `extendModules` definition ordering.

### Where R2 actually stands

Unchanged as a blocker, and now correctly scoped. `minimal`'s divergence is
genuine — it is called from a file, so none of the above applies to it. nix's
side re-measured today and is stable at
`szx145ay52y2lpmrj54y6l95vlznhpk6-nixos-system-minimal-…drv`, the same value
recorded when the divergence was first seen. The next probe is a file-based
control on the SAME shape, running now.

## §V.13 The file-based control DIVERGES — the artifact was real but is NOT the drvPath cause

§V.12 closed on "the next probe is a file-based control on the SAME shape,
running now." It landed, and it constrains the story sharply:

```
nixosSystem called FROM A FILE (/tmp/r2.nix), same bootable minimal module
  nix: /nix/store/csw8ynyxvkidb63n2jkaxk9bjj0aqjvi-nixos-system-nixos-…drv
  sui: /nix/store/wgpvrn0rjdqa6xyrvgl9xjzlgshybm1k-nixos-system-nixos-…drv
  => DIVERGE
```

A file-based caller makes `unsafeGetAttrPos` agree on both engines (§V.12's
table), so `modulesLocation` agrees, so the `setDefaultModuleLocation` wrap
agrees. **The `--expr` artifact cannot apply here, and the drvPath still
differs.** Both statements therefore stand together, and neither cancels the
other:

- the ordering divergence measured via `--expr` WAS an artifact of that harness
  (§V.12 — the mechanism is proven, on nix alone, by toggling the wrap);
- the toplevel drvPath divergence is GENUINE and survives a file-based caller.

Stated plainly because the temptation runs the other way: finding one real bug in
the harness does not retire the finding the harness was chasing. §V.12 fixed how
R2 is MEASURED; it did not move R2.

**What this rules out as the drvPath cause:** everything in §V.12's ruled-out
list, plus module definition ordering *if* the ordering probe re-run from a file
matches (running; the two outcomes separate cleanly — ordering-matches means the
cause is downstream of module evaluation, ordering-diverges means §V.12's
artifact story is itself wrong).

**Why the on-disk diff was not available.** nix's `.drv` is in the store; sui's
is ABSENT — sui computes the path but does not write the ATerm (the store is
daemon-owned). So the next bisect goes through `SUI_EMIT_DRV`, which dumps sui's
ATerms to a directory, against `nix derivation show -r` for nix's graph. Match by
derivation NAME, then find the divergent derivation all of whose own inputs
agree — that is the FIRST divergence, and the only one worth reading by hand.

**Measurement cost, recorded because it shapes what is worth probing.** Every
number in §V.11–§V.13 came from `target/debug/sui`; there is no release build.
A full NixOS toplevel eval runs ~10 min on nix and substantially longer on a
debug sui, so each row of this table is a multi-minute wait and the corpus-style
sweeps are not affordable at this build profile. Building `--release` before the
next sweep is the cheap fix.

## §V.14 Root-cause chain for the ordering divergence — two bugs fixed, leak narrowed

The ordering divergence is genuine (§V.13). Traced, it runs:

```
sui reports NULL for unsafeGetAttrPos "modules" on nixosSystem's args attrset
  -> eval-config.nix:28 sets modulesLocation = null
  -> setDefaultModuleLocation is SKIPPED, so user modules are not wrapped
  -> collectModules' breadth-first genericClosure leaves them one level SHALLOWER
  -> NixOS option definition order permutes -> toplevel drvPath diverges
```

Measured on the minimal config: nix puts the user module FIRST, sui puts it
EIGHTH, behind seven `installer/tools/tools.nix` children.

### Two real position bugs found and FIXED on the way (both shipped, both tested)

1. **`//` dropped attr positions entirely.** `overlay()` builds its node with an
   empty position slot (deliberately — `//` is O(1) and lazy), but `pos_for()`
   read only that slot, so EVERY key of EVERY `//` result reported null. This
   matters directly: `lib.nixosSystem` ends in
   `{ …; modules = …; } // removeAttrs args [ "modules" ]`. Fixed at the lookup
   (`pos_entry` walks right-then-left, matching `//`'s own precedence) so `//`
   stays O(1). Verified against nix in both directions; regression test
   **red-run** against the pre-fix behaviour before being trusted.
2. **A fileless frame was skipped instead of masking.** `EVAL_FILE_STACK` was
   `Vec<PathBuf>` and could not represent "no source file", so a thunk captured
   in an `--expr` context pushed nothing and inherited the CALLEE's file where
   CppNix returns null. Now `Vec<Option<PathBuf>>`.

**Neither closed the ordering divergence** — they were real bugs on the path,
not the remaining one. Stated plainly so the fixes are not mistaken for a fix to
R2.

### Where the position leak actually is — narrowed by elimination

All measured post-fix, `unsafeGetAttrPos … .file`:

| source of the attrset | nix | sui | |
|---|---|---|---|
| literal in a local flake's `flake.nix` | store path | **local path** | non-null, PATH DIFFERS |
| literal returned from a closure in a flake | store path | local path | non-null |
| `a // removeAttrs b […]` inside a flake | store path | local path | non-null |
| `import` of a store-path file (`toFile`) | store path | store path | MATCH |
| **`nixpkgs.lib.nixosSystem`** | `…-source/flake.nix` | **NULL** | **DIVERGE** |
| **`nixpkgs.lib.mkOption`** | `…-source/lib/default.nix` | **NULL** | **DIVERGE** |
| **`nixpkgs.lib` (the flake output attr)** | `…-source/flake.nix` | **NULL** | **DIVERGE** |

So positions survive flakes, closures, `//`, and store-path sources — and are
lost across the whole of nixpkgs' EXTENDED `lib`. The remaining suspect is the
`lib.extend` / `makeExtensible` / `fix` construction (`extends f rattrs = self:
let super = rattrs self; in super // (f self super)`), reached through a
fix-point thunk rather than a direct `//` of two literals. Note `NixAttrs::update`
— the eager merge at `value.rs:2465` — also builds `AttrsInner::Flat(result)`
with an empty position slot, and unlike `overlay` there is no node left to walk;
that is the first place to look.

**A second, independent divergence surfaced by the same table:** for a LOCAL
flake sui reports the working-directory path where nix reports the copied
`/nix/store/…-source/…` path. Non-null on both, so it does not cause this
ordering bug, but it is a byte-visible difference wherever a `.file` reaches an
output (nixpkgs `lib/types.nix`'s `attrTag` puts `pos.file` straight into
`declarations`).

## §V.15 The leak is `inherit` — found, partially fixed, REVERTED as incomplete

§V.14 named `lib.extend`/`fix` as the suspect. It was wrong. The leak is
`inherit`:

```nix
let a = { x = 1; };
    b = { inherit (a) x; };   # line 2
    c = { inherit a; };       # line 3
  nix: inheritFrom=2  inheritPlain=3
  sui: inheritFrom=NULL  inheritPlain=NULL
```

`attach_attrset_positions` iterates `set.entries()` and matches only
`ast::Entry::AttrpathValue`, so `ast::Entry::Inherit` contributes nothing to the
position table. An `inherit` BINDS an attribute exactly as `x = …` does, and
CppNix gives each inherited name the position of its own ident.

That is why every nixpkgs `lib` key came back null: `lib/default.nix` re-exports
through `inherit (self.options) mkOption …`. It is a much larger blast radius
than `modulesLocation` — `lib/types.nix`'s `attrTag` puts `pos.file` directly
into `declarations`, so every inherited option's documentation is affected too.

**Attempted, MEASURED WRONG, and reverted in the same session.** Adding an
`Entry::Inherit` arm made the position non-null but at the WRONG LINE — sui
reported 1 and 1 where CppNix reports 2 and 3, i.e. the ident offsets are not
resolving the way `AttrpathValue`'s do. And it did NOT change nixpkgs' `lib`,
which still reports null, so a second mechanism is in play beyond the missing
arm.

**Reverted deliberately.** A wrong position is a NEW divergence, not a partial
fix: null is at least distinguishable, whereas a plausible-but-wrong line lands
in `declarations` and diverges bytes while looking correct. Shipping it would
have traded a known gap for a silent one. The next step is to find why the
inherit ident's offset resolves to line 1 — compare against `static_attr_offset`
on the `AttrpathValue` path, which does resolve correctly — and to find the
second mechanism that keeps nixpkgs' `lib` null even with the arm present.

## §V.16 A release build makes the measurement 15x cheaper — corpus sweeps are now affordable

§V.13 recorded that every number so far came from `target/debug/sui` with no
release build. Built one:

```
minimal toplevel drvPath, sui --no-vm
  debug:   ~10+ min   (the reason every row in V.11-V.15 was a multi-minute wait)
  release: 41 s
```

That is not a footnote — it is the difference between bisecting by hand and
running a differential corpus. Every sweep this document defers as "not
affordable at this build profile" should be re-priced against 41 s.

**R2 re-measured on the release binary — still divergent, and informatively so:**

```
nix: /nix/store/szx145ay52y2lpmrj54y6l95vlznhpk6-nixos-system-minimal-…drv
sui: /nix/store/adjbgkwapyw7f6w7g7jq2bsvmdpn6x8b-nixos-system-minimal-…drv
```

sui's value is **byte-identical to the pre-`//`-fix debug measurement**. So the
`//` position fix — real and shipped in v0.1.183 — changed nothing for this
config, which is exactly what §V.15 predicted once `inherit` was identified as
the operative leak. Two independent confirmations now agree: `//` was a genuine
bug on the path but not the one holding R2.

### Also measured: `FlakeRefSyntax::Sui => LocalPathOnly` in sentinela is STALE

sentinela (`sentinela-config/src/lib.rs:210`) restricts the sui driver to
local-path flake refs, which blocks selecting sui at all. Tested against the
release binary — all three forms resolve:

| ref form | result |
|---|---|
| `.#nixosConfigurations.minimal.config.system.stateVersion` | `25.11` |
| `path:<repo>#nixosConfigurations.…` | `25.11` |
| `<abs-path>#nixosConfigurations.…` | `25.11` |

So the restriction records a limitation sui no longer has. It is NOT yet flipped:
enabling sui selection while the toplevel drvPath still diverges would ship a
known-wrong path. The flip is gated on R2 going green, not on this measurement —
recorded here so the gate is the drvPath, not a stale belief about ref syntax.

## §V.17 R2 BISECTED to a single root: `system-path`, a pure reorder of 189 identical packages

With the release binary (§V.16) the derivation-graph bisect became affordable.
`SUI_EMIT_DRV` dumped sui's ATerms; `nix derivation show -r` dumped nix's graph
for the SAME config.

**Method note — a harness error caught and corrected mid-bisect.** The first run
compared nix's graph for the CONTROL toplevel against sui's ATerms for `minimal`
— two different configurations. It "found" `etc-hostname` differing
`text='nixos'` vs `text='minimal'`, which is just the two configs' hostnames, not
an engine divergence. Re-dumped nix's graph for `minimal` before drawing any
conclusion. Second correction: matching derivations by NAME conflates every
`fetchFromGitHub` result, all of which are named `source` — the first "root"
candidates were `snowballstem/snowball` vs `alex/pretend`, unrelated upstreams
sharing a generic name. Restricting to names that are unambiguous in BOTH graphs
is what made the bisect readable.

**Result, apples-to-apples on `minimal`:**

```
sui drvs 3095   nix drvs 2989   BYTE-IDENTICAL 2980  (99.7% of nix)
unambiguously-named in both: 2455        divergent among them: 7
```

Of those 7, exactly ONE has zero mismatched inputs — every one of its own inputs
agrees with nix, so it is the FIRST divergence and the other six are cascade:

| derivation | mismatched inputs |
|---|---|
| **`system-path`** | **0 / 77** |
| `X-Restart-Triggers-dbus` | 1 / 3 |
| `dbus-1` | 1 / 6 |
| `user-units` | 1 / 14 |
| `system-units` | 1 / 81 |
| `nixos-system-minimal-…` (toplevel) | 2 / 33 |
| `etc` | 4 / 90 |

**And `system-path` diverges by ORDER ALONE:**

```
nix pkgs entries = 189   sui pkgs entries = 189   same MULTISET = True
first difference at index 0:
  [0] nix=rust_banken-0.1.15-aarch64-unknown-linux-musl   sui=nixos-version
  [1] nix=nixos-version                                   sui=nixos-rebuild-ng-25.11
```

Same 189 packages, permuted. `banken` is contributed by `fleetUniversalModules`
— i.e. by a USER module, the exact class `setDefaultModuleLocation` wraps.

### The chain, now measured at every link

```
attr positions lost (inherit; // before v0.1.183)
  -> unsafeGetAttrPos "modules" is null on nixosSystem's args
  -> eval-config.nix:28 modulesLocation = null
  -> setDefaultModuleLocation skipped; user modules NOT wrapped
  -> genericClosure (breadth-first) places them one level shallower
  -> environment.systemPackages definition order permutes   [MEASURED §V.13]
  -> system-path `pkgs` reorders, 189 identical packages    [MEASURED here]
  -> etc / system-units / toplevel cascade                  [MEASURED here]
  -> R2 red
```

This is the `{ imports = … }` demotion trap the nix repo's own CLAUDE.md records
("same 62 packages, 4 displaced — which moved system-path -> etc -> toplevel"),
independently rediscovered from the derivation side.

**What this buys: R2 is now a ONE-ROOT problem with a 41s falsification test.**
Fix the position leak, re-run, and `system-path` either matches or it does not —
no further guessing about which of 2989 derivations to look at. 99.7% of the
graph is already byte-identical, which is the strongest evidence to date that
sui's derivation construction is correct and the residue is this single
ordering defect.

## §V.18 R2 IS GREEN — root found, fixed, and verified on BOTH engines

```
.#nixosConfigurations.minimal.config.system.build.toplevel.drvPath
  nix          szx145ay52y2lpmrj54y6l95vlznhpk6-nixos-system-minimal-…drv
  sui --no-vm  szx145ay52y2lpmrj54y6l95vlznhpk6-…   MATCH
  sui (VM,     szx145ay52y2lpmrj54y6l95vlznhpk6-…   MATCH   <- the DEFAULT engine
      default)
whole graph: 2989 / 2989 nix derivations BYTE-IDENTICAL (was 2980)
             0 divergent of 2455 unambiguously-named (was 7)
sui-eval suite: 1392 passed, 0 failed
```

### The root: `as_flat()` freed the storage `pos_entry` reads

`as_flat`'s "RELEASE the parents" optimization swapped a flattened overlay's
`left`/`right` for empty attrsets. Byte-neutral for VALUES — every value reader
routes through the cache — but `pos_entry` is the ONE consumer that reads
`left`/`right`, so the release discarded every `AttrPositions` table beneath it:

```
fresh overlay        nix line 1, sui line 1
after ONE attr read  nix line 1, sui NULL
```

That is why §V.14's `//` fix passed in isolation and did nothing for nixpkgs:
nixpkgs READS from an attrset long before anyone asks for a position. It also
fired at CONSTRUCTION for a nested `//`, since `overlay()` calls `is_empty()`
which routes through `as_flat()`.

**Fix — `position_husk`:** release the values, keep the position skeleton (same
tree shape, keys mapped to `Value::Null`). The tables are small and exist only on
attrset literals; `Null` is inline, so every `Rc` the real values held is still
dropped. The memory win is retained, the walk survives.

### Two more roots fixed in the same pass

**`pos::line_col` was a CONSTANT `(1, offset + 1)`** — no offset→line/column
conversion existed; `resolve` fetched the file text and discarded it. The doc
comment claimed "Verified against `nix eval`" and cited three numbers that were
**this function's own output recorded as the oracle's**, pinned green by two unit
tests, so the bug was structurally incapable of going red. Real answers on that
exact fixture: `2:3 / 3:3 / 4:3`, not `1:5 / 1:18 / 1:31`. Now resolved properly,
with BYTE columns (measured: a 2-byte `é` advances CppNix's column by 2). Three
tests re-baselined against nix rather than deleted. **Not on the
`modulesLocation` path** — `eval-config.nix:28` reads `.file` only — so this is
independent of R2 and reaches the other `unsafeGetAttrPos` consumers.

**`//` returned the LOSER's position.** The right-then-left walk added in
§V.14 fell through on `None`, so `a // (mapAttrs … )` reported the shadowed
literal's position where CppNix reports null. CppNix copies each attribute WITH
its position, so the winner's verdict stands even when it is "no position". This
is the INVERSE error on the `modulesLocation` path — an invented position makes
sui wrap where nix does not. Fixed; found by the fan-out, by no existing test.

### Correction to §V.15

§V.15 recorded the `inherit` arm as "measured wrong" because it produced line 1.
That diagnosis was wrong: EVERY position reported line 1, because of the
`line_col` constant above. The arm was fine. `inherit` remains genuinely
unhandled (`attach_attrset_positions` matches only `Entry::AttrpathValue`) and is
now cleanly separable work — it is NOT what was blocking R2.

### Contradiction left open, honestly

The fan-out's VM axis reported that the bytecode VM "has no attribute-position
machinery at all", which would make every fix here invisible to the shipped
engine. **Measured otherwise:** the default engine produces the byte-identical
toplevel drvPath. Either the VM shares this code or flake/module evaluation
routes through the tree-walker. Not yet resolved — the measurement is trusted
over the code read, and the code read is not dismissed.

## §VI. Independent of the plan — today

`sui store gc` reads neither `temproots` nor runtime roots, and accepts
`--max-age-days` then ignores it (`traits.rs:157-162`; `local.rs:384-483`). It
operates on the real `/nix/var/nix/db/db.sqlite` with no busy-timeout and no
transaction. **It will delete live bytes from under a running build.** Guard it
now; it is not part of any rung.

## §VII. Tier-honest bottom line

**True today:** R0 and R1 are green. sui's evaluator is mature; `realize_via_daemon`
converges a darwin node *by handing the build to CppNix's daemon*.

**A plan:** everything in §V.

**A hope:** that R2's divergence is one root and not a family. Unknown until R3/R4
exist.

**Resolved since first writing:** the columnar-attrset remedy **never landed**
(§V.1) — HAMT → hashbrown shipped in its place. The cid-toplevel OOM was
re-measured 2026-08-11: release ran 24 min without a result driving swap
13 → 41 GB; debug died of **stack overflow at 508 MB RSS**, a failure mode no
doc records and which §V.1 now reproduces in seconds on `minimal`.

**Still unverified:** whether the `minimal` overflow and the cid OOM are one
defect or two. They are assumed related here and that assumption is NOT
evidence.
