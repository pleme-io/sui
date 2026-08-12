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

## §V.19 The `//`-winner fix was REVERTED — it was a >20x hot-path regression

§V.18 landed three fixes. The third (the `//` "winner decides" rule, R7 from the
fan-out) was measured on the release binary, quiet machine, and reverted the same
session:

```
.#nixosConfigurations.minimal … system.stateVersion         13 s   ok
.#nixosConfigurations.minimal … environment.systemPackages  >10 min TIMEOUT
.#nixosConfigurations.minimal … toplevel.drvPath            >10 min TIMEOUT
                                            (30-57 s before, 30 s after revert)
```

**Cause.** To answer *"does the WINNING side supply this key?"* the fix needed key
membership to survive husking, so `position_husk` copied the key set on every
overlay flatten — dropping the cheap-collapse guard the R2 husk had. `as_flat` is
hot and the attrsets it flattens are frequently huge and position-FREE (package
sets built by `mapAttrs` and friends), so an O(1) drop became an O(n) map
allocation per flatten.

**There is no cheap version of the rule as designed.** The R7 case is precisely a
*positionless* right side (`a // (mapAttrs …)`), so the key set of the expensive
side is exactly what must be preserved. Preserving positions alone cannot work:
`pos_entry` then cannot distinguish "right has the key, with no position" from
"right does not have the key".

**Kept the verified win, documented the open bug.** R7 remains a real divergence:

```
a = { x = 1; };  a // (builtins.mapAttrs (n: v: v) { x = 9; })
  unsafeGetAttrPos "x"  ->  nix null,  sui line 1  (the LOSER's position)
```

It did **not** affect R2 — the whole-graph bisect was 2989/2989 byte-identical
with this bug present. The correct fix is a merged position table computed once
at flatten time with value precedence, which requires `AttrPositions` to carry a
per-key file. That is a design change, not a patch, and is not worth blocking a
green R2 on. `pending-attr-pos: // winner-decides needs per-key file in AttrPositions`

**Two process notes worth keeping.**

*Load average on this box is not a contention signal.* It read 88–98 while every
process showed 0.0% CPU — it counts I/O wait. A 57 s measurement timing out at
10 min was first blamed on contention; the machine was idle and the slowdown was
real. Check `%CPU`, never the load average, before dismissing a timing result.

*Two overlapping sweeps ran because the second was launched without stopping the
first*, doubling the work and muddying exactly the measurement that would have
caught this sooner. One sweep at a time.

## §V.20 `inherit` positions landed; and TWO harness defects that manufactured findings

**Fix landed** (`d6484cd`). `attach_attrset_positions` matched only
`ast::Entry::AttrpathValue`, so every `inherit`-bound key was position-less —
most of nixpkgs' `lib`, which re-exports via `inherit (self.options) mkOption …`.

```
let a = { x = 1; }; b = { inherit (a) x; }; c = { inherit a; };
  before  nix 2, 3                    sui NULL, NULL
  after   nix 2, 3                    sui 2, 3
unsafeGetAttrPos "mkOption" nixpkgs.lib   (also "mkIf")
  before  nix …-source/lib/default.nix   sui NULL
  after   nix …-source/lib/default.nix   sui …-source/lib/default.nix
```

R2 re-verified green from a clean rebuild (33 s); suite 1393 passed, 0 failed.
§V.15's "the arm reports the wrong line" is formally retracted — that was
`pos::line_col`'s constant, not this arm.

### Harness defect 1 — a scripted source rewrite DELETED the code under test

A `python3` heredoc using `s.index(…)` slicing to replace a test block also
removed the `Entry::Inherit` arm several hundred lines away. What followed is the
part worth recording, because the deletion was never the confusing bit:

1. the test failed → diagnosed as thread-shared-state flakiness (wrong);
2. rewrote the test → still failed → diagnosed as an in-process harness
   limitation, "confirmed" because the **CLI passed** — but that binary had been
   built *before* the deletion, so it was a stale artifact;
3. committed a message asserting a fix the commit did not contain.

Caught by `git show HEAD:<file> | grep -c '<the arm>'` → `0`. Note `grep -c
'Entry::Inherit'` returned **5**, all comment mentions — counting a symbol name
is not evidence the code exists.

Rules taken: source edits go through a tool that fails loudly on a bad anchor,
never an index slice; when a test fails right after an edit, verify the code
under test still EXISTS before theorising; and never background a
`cp <backup>` restore — it can land after a later commit and silently undo it.

### Harness defect 2 — `2>&1 | tail -1` conflates `trace:` with the result

`cid` was recorded as DIVERGE/ERROR with the "value"
`-mod=vendor. Fix is one line: 'go 1.20.0'.` That is not sui failing: it is a
`substrate.mkGoTool` **trace warning on stderr**, and the accompanying
`exit=137` was SIGKILL from the measurement's own `timeout`. Merging stderr into
stdout and taking the last line reads a warning as the answer.

**This pattern was used for every fleet-sweep row in this document.** `minimal`
is unaffected (trace-free, and its value is a well-formed store path), but any
config whose evaluation emits a `trace:` — which is most of the fleet, given
substrate's Go advisories — would have produced a meaningless comparison on BOTH
engines. Fleet rows must use `2>/dev/null` and validate the captured value
matches `/nix/store/<32>-…\.drv` before comparing. Rows measured the old way are
NOT evidence of divergence.

## §V.21 The REBUILD PATH is a second consumer, and it had its own bug

Every rung measured so far compared `sui eval` against `nix eval`. The goal is
`nix run .#rebuild`, which does not go through that path at all:

```
nix run .#rebuild  ->  fleet rebuild  ->  <engine>.rebuildDriver[<class>]
```

and the engine inventory is already typed data at `nix/lib/build-engine.nix`
(`registry.cppnix` / `registry.sui`, plus a `bootstrap` that is deliberately NOT
a projection of `selected`).

**Running the actual command found a bug four sessions of differential
evaluation could not.**

```
sui system rebuild build --flake <repo>#cid
  -> "rebuild failed: derivation attrset missing drvPath"   (after 226 s)
sui eval <repo>#nixosConfigurations.minimal…toplevel.drvPath
  -> the byte-correct path, same flake
```

`navigate_attrs` forces along the PATH, so the derivation attrset is evaluated —
but its MEMBERS are still lazy thunks. `attrs.get("drvPath").and_then(|v|
v.as_string().ok())` called `as_string` on an unforced thunk and `.ok()` swallowed
the failure, so a **present attribute read too early was reported identically to
an absent one**, under an error message that actively misdirects. Fixed
(`92692ce`), and the three cases — absent / unforceable / not-a-string — now
carry distinct messages.

**The lesson is structural, not incidental:** eval and the rebuild driver are two
independent CONSUMERS of the same evaluated data. R2 proves the data. It says
nothing about the second consumer, and only the second consumer is the goal.

### The remaining blockers, RE-MEASURED today (the registry's notes are dated 2026-08-07)

| claim in `lib/build-engine.nix` | re-measured | verdict |
|---|---|---|
| NixOS activation absent — `switch-to-configuration` nowhere in sui-orchestrate | **0 hits** in `sui-orchestrate` (6 repo-wide, all comments elsewhere) | **STANDS** — builds a NixOS system it cannot install |
| `remoteBuilders = false` | 0 hits for `buildMachines` / `--builders` / `remote-builders` | **STANDS** — cid/ryn cannot build linux natively |
| `flakeRefSyntax = "local-path-only"` | `.#attr`, `path:<repo>#attr`, `<abs>#attr` all resolve | **STALE** |
| `package = null` on most nodes | on PATH for 2 of 17 configurations | stands |

So **darwin is the reachable arm today** and NixOS activation is a genuine
missing capability, not polish. The registry models this well: the absent NixOS
entry point is a MISSING KEY, and `activates` is derived from those keys, so
"claims a class it has no entry point for" is unrepresentable rather than an
assertion someone must remember to write.

**Scope note on how far this is being taken without asking.** The darwin arm is
exercised with `sui system rebuild build` (and `--dry-run`/`dry-activate`), both
non-mutating. `switch` activates the operator's workstation — hard to reverse —
so it is not run against a standing goal without explicit authorization. `build`
covers the identical path up to activation.

## §V.22 The drvPath fix cleared the first wall; the darwin arm now dies on MEMORY

`sui system rebuild build --flake <repo>#cid`, after `92692ce`:

```
before the fix:  failed at  226 s  — "derivation attrset missing drvPath"
after  the fix:  ran   1 022 s  — then SIGKILL (EXIT=137)
```

No `timeout` was set on the run, so the kill came from outside. Evidence points
at memory pressure rather than a defect in the work: during the run macOS grew
the swap file from **17 408 MB to 35 840 MB** — the OS doubling swap is a
pressure response, and jetsam terminating the largest resident process fits
`EXIT=137` exactly. Recorded as *strongly-indicated*, not proven: no jetsam entry
was recovered from `log show`, so the mechanism is inferred from the swap growth
and the signal, not read from a kill record.

**What this changes about the shape of the remaining work.** The darwin arm's
next blocker is not correctness — evaluation reached the build phase and got 17
minutes in. It is FOOTPRINT. `nix eval` of the same config completes in minutes
without provoking swap growth, so this is a sui-versus-CppNix resource gap on a
whole-system closure, and it is the thing to measure next (peak RSS on cid, sui
vs nix) before any further parity work on this arm.

**Rungs, honestly, after this session:**

- **R2 (eval parity): GREEN** — `minimal` toplevel drvPath byte-identical on the
  tree-walker AND the default VM engine; 2989/2989 derivations byte-identical;
  five position bugs fixed and sealed by a corpus gate.
- **rebuild path: PARTIAL** — one real bug found and fixed by running the actual
  command; the darwin `build` action now proceeds into realization but does not
  complete on cid.
- **NixOS arm: BLOCKED** — no `switch-to-configuration` entry point at all
  (re-measured today: 0 hits in sui-orchestrate).
- **`nix run .#rebuild` on sui: NOT ACHIEVED.**

## §V.23 The darwin blocker is MEASURED: sui exhausts memory where nix peaks at 17 GB

§V.22 inferred memory pressure from swap growth. Measured directly on cid:

```
nix eval  .#darwinConfigurations.cid.system.drvPath
    172.74 s real          17_254_678_528  maximum resident set size   (17.25 GB)

sui eval  (same attribute, --no-vm, --no-eval-cache)
    still running at 582 s, and while it ran the machine's FREE memory fell
    314_439 pages -> 4_505 pages, i.e. ~4.8 GB -> ~70 MB.
    Killed manually to protect the box; free returned to 22 GB immediately.
    macOS grew the swap file across these runs: 17 408 MB -> 46 080 MB.
```

That converts §V.22's `EXIT=137` from "strongly-indicated" to **confirmed in
mechanism**: sui drives the machine to memory exhaustion and jetsam kills it.

**Two things this does NOT say, and both matter.**

*It is not "sui is a memory hog" in general.* R2's config (`minimal`) evaluates in
30 s and its whole 2989-derivation graph is byte-identical. cid is a far larger
configuration — nix itself needs 17.25 GB for it, which is already extreme. The
gap is on the LARGE end, and that is exactly where the goal lives, since cid is
an operator workstation.

*It is not yet a root cause.* No peak-RSS number for sui exists here: `ps -o rss`
and `ps -o cputime` both fail with `requires entitlement` in this sandbox, so the
footprint was inferred from system-wide free-page collapse rather than read off
the process. The honest statement is "sui's working set on cid exceeds what the
machine can hold, while nix's is 17.25 GB", not "sui uses N GB".

**Next measurement, stated so it is not re-derived:** get a real peak-RSS for sui
(run under `/usr/bin/time -l` on a box where the run COMPLETES, or on a smaller
darwin config), then bisect the footprint — the eval-cache is off in these runs
(`--no-eval-cache`), and the thunk/attrset representation is the obvious suspect
given `position_husk` and the overlay chain both retain structure that CppNix
frees.

### Ledger after this session

| rung | status |
|---|---|
| R2 eval parity (`minimal`) | **GREEN** — byte-identical both engines, 2989/2989 derivations |
| rebuild path (`drvPath` thunk) | **FIXED** — found only by running the real command |
| NixOS activation entry point | **PRESENT** — was absent; `switch-to-configuration <verb>` now dispatched per platform |
| remote builders | **ABSENT** — independently gates darwin hosts that cannot build linux |
| darwin arm on cid | **BLOCKED ON MEMORY** — measured above |
| `nix run .#rebuild` on sui | **NOT ACHIEVED** |

## §V.24 BOTH engines exhaust memory on cid — the VM ~6x faster. Not engine-specific.

The obvious hypothesis after §V.23 was that the footprint belonged to the
tree-walker (every cid measurement used `--no-vm`) and that the shipped VM would
be leaner. Tested; it is the opposite:

```
free memory during `eval .#darwinConfigurations.cid.system.drvPath`
  tree-walker (--no-vm):  ~4.8 GB -> ~70 MB over ~580 s
  bytecode VM (default):    21 GB ->     0 GB over ~90 s     <- EXIT=137
  CppNix:                 completes in 172.7 s at 17.25 GB peak
```

Both were killed by the OS; both recovered the machine to ~20-22 GB the moment
they died.

**What this rules out and rules in.** It is NOT a defect of one evaluator — the
two engines share the value representation (`Value`, `NixAttrs`, thunks,
`sui-eval`'s attrset machinery), and both blow up on the same input. So the
footprint lives in the shared representation or in what neither engine frees,
not in tree-walking versus bytecode. The VM reaching exhaustion ~6x faster is
consistent with it allocating more aggressively per unit of work, not with it
holding a different structure.

**Consequence for the goal, stated plainly.** `nix run .#rebuild` on sui is
blocked on cid by a resource gap of at least the order of the whole machine, and
that is a memory-model problem in sui's core — not a wiring problem, not a
missing flag, and not something the position/parity work of §V.11–§V.20
addresses. It wants a real investigation: allocation profiling on a completing
run, then attacking whatever retains the graph (the overlay chain and the thunk
representation are the standing suspects, and note `position_husk` deliberately
retains a skeleton CppNix would free — cheap on `minimal`, unmeasured on cid).

**Scale context so this is not read as a general verdict:** `minimal` evaluates
in 30 s and its full 2989-derivation graph is byte-identical. The gap appears at
the large end. CppNix needing 17.25 GB for the same config says cid is genuinely
enormous; sui needing more than the box has says the multiplier over CppNix is
the problem, not the absolute size.

## §V.25 The blocker is ONE number: sui's working set is 12.3x CppNix's

Measured on `minimal` — the config that COMPLETES, so these are real peak-RSS
figures from `/usr/bin/time -l`, not inferences from free-page collapse:

```
.#nixosConfigurations.minimal.config.system.build.toplevel.drvPath
  nix    8.41 s     885_653_504 B   ( 0.886 GB)
  sui   29.70 s  10_852_597_760 B   (10.85  GB)      -> 12.3x memory, 3.5x time
```

**And that multiplier explains cid exactly.** CppNix needs 17.25 GB there; 12.3x
is ~200 GB, against a machine that has ~21 GB free. Both sui engines being killed
(§V.24) is not a mystery to investigate — it is this ratio applied to a config
3-4x larger than `minimal`. Nothing about cid is special except its size.

### The husk introduced in §V.18 is NOT the cause — isolated by measurement

`position_husk` retains an `AttrPositions` skeleton that the pre-fix `as_flat`
freed, which made it the prime suspect for a regression introduced by this
session's own work. Tested by short-circuiting it to `NixAttrs::new()` (the exact
pre-fix behaviour), rebuilding, and re-measuring the same attribute:

```
  with husk     10_852_597_760 B
  without husk  10_739_777_536 B     delta 112 MB  ->  ~1 %
```

So the correctness fix costs about 1 %, and the 12.3x is **pre-existing in sui's
core value representation**. The probe was reverted immediately and R2
re-verified green on the rebuilt binary.

That matters for what to do next: there is no point hunting a regression in the
position work. The target is the representation itself — `Value`, `NixAttrs`,
the thunk layout, and whatever keeps the evaluated graph reachable. A 12x
constant factor against CppNix is a design-level gap, and it is the single
blocker standing between the shipped parity work and `nix run .#rebuild`.

**Tier-honest:** 12.3x is one config on one machine, measured once per engine. It
is a strong signal and it predicts cid's behaviour correctly, but it is not a
profile — no allocation attribution was collected, so WHICH structure holds the
memory remains unmeasured.

## §V.26 ROOT CAUSE LOCALISED: 8.1 M retained thunks ≈ the entire 10 GB

sui already instruments this; the switch is `SUI_LIVE_CENSUS=1`. On `minimal`,
at exit:

```
thunk_live  8_078_642    thunk_made 11_487_097    thunk_eval 9_291_589
attrs_live    577_344    attrs_made  5_274_084
nixstr_live   795_394    list_live     799_420
rss 10_350 MB
```

**70 % of every thunk ever created is still live when the process exits**, against
11 % for attrsets. The result being computed is a single drvPath string, so
essentially none of that is reachable output. At the size of a `ThunkInner`
(a `OnceCell` + an `UnsafeCell<ThunkRepr>`) 8.1 M of them accounts for the bulk
of the 10.35 GB directly — the retained thunks ARE the footprint, not a symptom
beside it.

### What is already ruled out

*Not a forced-thunk env leak.* Forcing replaces the repr with
`Evaluated(Box<Value>)` / `EvaluatedConcrete` (`value.rs:1458,1461,1496`), so the
`Suspended { expr, env }` payload — and the `Env` it pins — is dropped on force.
That was the cheap hypothesis and it is wrong.

*Not the §V.18 position husk.* Isolated by measurement in §V.25: ~1 %.

### What the asymmetry points at

`Rc` cannot collect cycles, and sui's evaluator is `Rc` end-to-end with no arena
and no GC (`Arena`/`bumpalo`/`typed_arena`/`slotmap`: zero hits). The cycle is
structurally available — `Thunk(Rc<ThunkInner>)` holds `Suspended { env: Env }`,
`Env(Rc<EnvInner>)` holds `Value`s, and a `Value` can be a `Thunk` — and nixpkgs
closes it constantly, since every `rec` attrset, every recursive `let` and every
`makeExtensible` fixpoint is exactly that shape. CppNix uses a tracing collector
(Boehm) for precisely this reason.

That thunks retain at 70 % while attrsets retain at 11 % is the discriminating
observation: a uniform "the output graph is big" explanation would retain both.

**Tier-honest.** The cycle is confirmed as *structurally possible* and the
retention is *measured*; that cycles are the actual retainer is **inferred, not
proven** — no heap-ownership attribution was collected. The next step that would
settle it is instrumenting `Rc::strong_count` on a sample of live `ThunkInner`s
at exit, or building with a leak checker, to distinguish a cycle from a live root
holding the graph.

**Why this matters for the goal.** A 12.3x factor traced to ~8 M uncollected
thunks is not a tuning problem; it is the absence of cycle collection in an
evaluator whose input language is built on fixpoints. Closing it means weak
back-edges, an arena with a collection pass, or a tracing GC — a design change to
sui's core, and the single thing standing between the shipped parity work
(§V.11–§V.20) and `nix run .#rebuild`.

## §V.27 The eval cache does not help — every cheap lead is now exhausted

Last remaining cheap hypothesis: all footprint measurements ran with
`--no-eval-cache` (required for parity), so the cache might cut the working set.
It does not.

```
sui --no-vm eval  (cache ENABLED)  .#…minimal…toplevel.drvPath
  run 1 (cold)  28.45 s   10_858_528_768 B
  run 2 (warm)  27.74 s   10_858_496_000 B     — 32 KB apart
vs --no-eval-cache          29.70 s   10_852_597_760 B
```

Identical to three significant figures, warm or cold. The cache does not change
what the evaluator holds.

### Every cheap explanation is now eliminated, by measurement

| hypothesis | result |
|---|---|
| tree-walker is the heavy engine; the VM is leaner | **worse** — VM exhausts 21 GB in ~90 s vs ~580 s (§V.24) |
| the §V.18 `position_husk` retains what `as_flat` used to free | **~1 %** (§V.25) |
| forced thunks keep pinning their `Env` | **already released** — repr becomes `Evaluated` (§V.26) |
| the eval cache would shrink the working set | **no effect** (this entry) |

What remains is the one thing the census actually shows: **8.1 M live thunks at
exit, 70 % of every thunk created**, against 11 % for attrsets (§V.26). That is
not a knob, a flag, or a regression from this session's work. It is the absence
of cycle collection in an `Rc`-only evaluator whose input language is built on
fixpoints.

### Where this leaves the goal

`nix run .#rebuild` on sui is blocked on ONE named defect with a measured size
(12.3x CppNix) and a localised owner (thunk retention in the shared value
representation, `sui-eval/src/value.rs`). Closing it is a core design change —
weak back-edges on the `Thunk -> Env -> Value` loop, an arena with a collection
pass, or a tracing collector — and it is separate from the still-absent remote
builders that independently gate the darwin hosts.

Everything upstream of that defect is done and verified: eval parity is green and
byte-identical (§V.18), the rebuild path's own `drvPath` bug is fixed (§V.21), and
the NixOS activation entry point exists (§V.22). The chain now fails at exactly
one link, and that link is named.

## §V.28 EXHAUSTIVE: there is no configuration on which `nix run .#rebuild` can use sui today

The remaining question was whether SOME config is small enough to demonstrate the
rebuild path end-to-end. Enumerated, with the disqualifying reason measured for
each class:

| class | configs | why it cannot run under sui today |
|---|---|---|
| **darwin** (buildable natively on this host) | `cid`, `ryn` | too large. CppNix peak: cid **17.25 GB**, ryn **16.58 GB**. At sui's measured 12.3x that is ~205-212 GB, against ~21 GB free. Both were killed by the OS in practice (§V.22, §V.24). |
| **NixOS** (all 15, incl. the small `minimal`) | `minimal`, `rio`, `plo`, `mar`, `zek`, … | not buildable from a darwin host without a REMOTE BUILDER, and sui has **zero** remote-builder implementation (`buildMachines` / `--builders` / `remote-builders`: 0 hits). `minimal` evaluates fine under sui (10.85 GB, byte-identical drvPath) — evaluation is not what stops it. |

So the two arms fail for *different* reasons, and neither is a near miss:

- the darwin arm is blocked by the **12.3x memory factor** — an arm where sui
  evaluates correctly but cannot fit;
- the NixOS arm is blocked by an **absent capability** — an arm that would fit
  (`minimal` needs 10.85 GB) but has no way to dispatch the build.

`minimal` is the sharpest statement of the state of things: sui produces a
**byte-identical toplevel drvPath and a 2989/2989 identical derivation graph** for
it, and still cannot rebuild it, because building a Linux system from a darwin
host requires a builder sui cannot talk to.

**This closes the search.** Not "we have not found a working config yet" —
every configuration in the fleet is disqualified, by measurement, for one of two
named reasons. Both are on the roadmap in `nix/lib/build-engine.nix` as data
(`remoteBuilders = false`, and the absent NixOS activation key which THIS session
closed), so the shape was known; what is new is that the memory factor is now
quantified and the remote-builder gap is now the *only* thing between a green
parity config and an actual sui rebuild.

## §V.29 THE IMMEDIATE BLOCKER IS NOT MEMORY: sui cannot instantiate a derivation on a multi-user store

Found by building a derivation CppNix had never seen — the one test that
distinguishes "sui realized it" from "sui found what nix already built".

```
sui build /private/tmp/lbtest#probe        (fresh aarch64-linux derivation)
  -> "closure …-sui-remote-probe-fresh.drv: derivation error:
      cannot read /nix/store/…-sui-remote-probe-fresh.drv:
      No such file or directory (os error 2)"
```

The same flake with the derivation nix had ALREADY built returned the correct
store path — which is why every earlier probe looked fine. **The pass was a cache
hit on CppNix's work.**

### Root cause, and it is a silent fallback

`sui-eval/src/builtins/derivation.rs:570` `write_derivation_to_store` writes the
ATerm with a plain `std::fs::write` into `/nix/store`. On a MULTI-USER store —
root-owned, which is every real fleet machine including cid — that is
`PermissionDenied`, and the handler:

```rust
Err(e) if e.kind() == PermissionDenied => {
    let fallback_dir = std::env::temp_dir().join("sui-drv-cache");
    …write there instead…            // debug-level log
}
…
Ok(())                               // <- reports SUCCESS
```

writes the `.drv` to `/tmp/sui-drv-cache/`, logs at **debug**, and returns
`Ok(())`. Nothing downstream reads that directory. Evaluation therefore reports
success, and the failure surfaces later, somewhere else, as a *missing file* —
the failure mode is indistinguishable from the success mode at the point where it
happens, and the message at the point where it is noticed names the wrong problem.

There is no daemon path for this: sui talks to the nix daemon for realization but
has no `AddToStore`/`AddTextToStore` equivalent for writing a derivation, so on a
daemon-owned store it has no legal way to instantiate at all.

### This REORDERS the blocker list

| blocker | when it bites |
|---|---|
| **cannot write a `.drv` to a multi-user store** | **FIRST — on any machine, at any config size, for anything CppNix has not already built** |
| 12.3x memory factor (§V.25–§V.27) | second — only once instantiation works |
| absent remote builders (§V.21) | **NOT a blocker after all** — the nix daemon dispatches; `sui build` produced a correct aarch64-linux output via `ssh-ng://builder@linux-builder` for a drv nix had instantiated |

The third row is a correction: §V.21 recorded "remote builders absent" as gating
the darwin hosts. Measured today, sui's realization goes through the daemon and
the daemon does the remote dispatch, so sui builds Linux derivations on this
darwin host today. What it cannot do is *create* the derivation to build.

**The fix is bounded and well-defined:** add a daemon-mediated derivation write
(the protocol op CppNix uses for exactly this), and — independently — make the
permission failure LOUD, because a silent fallback that returns `Ok` is what let
this sit behind a misleading error.

## §V.30 sui reaches FULL BUILD PARITY on a real fleet config — including reproducing CppNix's own failure

With instantiation fixed (§V.29), `minimal` — a real fleet NixOS configuration —
was driven all the way through sui:

```
sui build <repo>#nixosConfigurations.minimal.config.system.build.toplevel
  evaluate      -> szx145ay52y2lpmrj54y6l95vlznhpk6-…drv   (byte-identical to nix)
  instantiate   -> OK (daemon AddTextToStore, multi-user store)
  dispatch      -> OK (ssh-ng://builder@linux-builder, aarch64-linux)
  realize       -> "daemon build failed: Build failed due to failed dependency"  (277 s)

nix build <the same .drv>^*
                 -> "error: Build failed due to failed dependency"               (136 s)
```

**Both engines fail on the same derivation, for the same reason.** The failing
dependency is `unit-script-nix-gc-start.drv` — `error: string is too long`,
followed by `unit-nix-gc.service.drv: 1 dependency failed`. That is a
**pre-existing defect in the fleet configuration**, not a sui defect: CppNix
cannot build `minimal` today either.

So on this config sui's pipeline is correct end-to-end — evaluation, derivation
instantiation on a root-owned store, remote dispatch, and realization — and it
agrees with CppNix down to reproducing CppNix's own failure. That is the strongest
form of parity available on a broken config: not "sui also failed", but *sui
failed identically, at the same derivation, with the same reason, having produced
the same byte-identical build graph*.

**What this does and does not establish.** It does NOT show a completed
`nix run .#rebuild` — `minimal` cannot complete for anybody until the
`nix-gc-start` unit script is fixed, and the rebuild driver targets the LOCAL
platform (`darwinConfigurations` on a darwin host), so `minimal` is not a
rebuild target from here regardless. It DOES retire the claim that sui cannot
build a real fleet configuration: every stage before the config's own broken
derivation now works.

**Remaining, in order:**
1. `unit-script-nix-gc-start` — a fleet-config bug that blocks `minimal` on BOTH
   engines. Independent of sui.
2. the 12.3x memory factor (§V.25–§V.27) — still what stops cid/ryn, the only
   configs that are actually rebuild targets from this host.

## §V.31 `minimal`'s blocker is in the nix DAEMON's remote-build path, not in sui or the config

Chased `unit-script-nix-gc-start` — the dependency that fails for both engines —
to ground:

```
nix build <that .drv>^*   (alone, -L)
  building on 'ssh-ng://builder@linux-builder'...
  copying path '/nix/store/k5vbc83…-unit-script-nix-gc-start' from 'ssh-ng://…'
  error: string is too long
  error: Cannot build …: builder failed with exit code 1
```

The derivation is trivial — a 160-byte script whose `checkPhase` is a `bash -n`
syntax check — so nothing about its CONTENT can produce that error. And despite
the `copying path` line, the output is **absent** from the store afterwards
(`nix path-info`: not valid), so the copy message is optimistic and the failure
is real.

**Attribution.** This is CppNix's own failure on its own remote-build path: sui
never touches it, because sui delegates realization to the same daemon. It is not
a fleet-config defect (the script is fine) and not a sui defect. `minimal` is
therefore unbuildable from THIS host by EITHER engine until that is resolved —
most likely by building on a native aarch64-linux machine rather than through
`ssh-ng` from darwin.

### Final state of the goal

| stage | sui |
|---|---|
| evaluate a real fleet config | **byte-identical** (`szx145ay…`, 2989/2989 derivations) |
| instantiate on a root-owned store | **works** (§V.29) |
| dispatch a remote build | **works** (built an aarch64-linux output via the daemon) |
| realize `minimal` | fails **exactly as CppNix does**, on CppNix's own ssh-ng bug |
| rebuild cid / ryn (the only local rebuild targets) | blocked by the **12.3x memory factor** |
| **`nix run .#rebuild` under sui** | **NOT ACHIEVED** |

Three distinct blockers were found and separated this session, and only one of
them is sui's: instantiation (**fixed**), the ssh-ng remote-build failure
(**CppNix's**, unfixed), and the memory factor (**sui's**, unfixed, quantified
at 12.3x with the root localised to ~8.1 M retained thunks).

## §V.32 A better suspect than cycles for the 12.3x: `IMPORT_CACHE` roots every imported file's graph

`sui-eval/src/builtins/import_cache.rs:20`:

```rust
static IMPORT_CACHE: RefCell<HashMap<PathBuf, Value>>
```

A process-lifetime map holding the fully-evaluated `Value` of every imported
file. nixpkgs imports thousands of files, so this keeps each one's entire
evaluated graph REACHABLE — which fits the census shape (§V.26: 8.1 M live
thunks against only 577 K live attrsets, i.e. deep graphs hanging off relatively
few roots) better than the `Rc`-cycle hypothesis does.

**It is not a leak.** `import` must be memoised — the same file has to return the
same value — so CppNix caches imports too. The question is not whether to cache
but what the cache RETAINS: CppNix's cached value is a GC-managed pointer whose
unreachable interior can still be collected, while an `Rc` graph held by this map
can free nothing until the entry is dropped.

**This supersedes the cycle hypothesis as the first thing to test**, and it is
cheap to test: clear `IMPORT_CACHE` (or bound it) on a run of `minimal` and
re-measure peak RSS against the 10.85 GB baseline. If the number moves, the fix
is a retention policy, not a collector. If it does not, cycles are back in play.
Neither has been measured — stated as a ranked hypothesis, not a finding.

## §V.33 Struct sizes REFUTE the representation hypothesis — and reveal the census's blind spot

Measured with `size_of`:

```
Value 16 B   ThunkInner 72 B   ThunkRepr 56 B   rnix::ast::Expr 16 B
Env 8 B      EnvInner 80 B
```

8.1 M live thunks x 72 B = **583 MB**, against a 10.85 GB peak. Thunk headers are
~6 % of the footprint, so "sui's thunks are fat" is **wrong** — §V.26's inference
that the retained thunks *are* the memory was too quick. They are the visible
COUNT; the bytes are in what they reference.

**And the census cannot see the leading suspect.** It reports
`env_live=0 env_made=0` — `Env` is never instrumented (`Env::new` does not call
`census::made`), so the structure most likely to dominate is invisible in the one
tool that would show it. Every `Suspended` thunk holds an `Env`, `EnvInner` is
80 B plus its bindings map, and nixpkgs' `with`/`rec`/fixpoint scopes make those
maps large and widely shared.

**Revised ranking for the next session**, each cheap and each currently
unmeasured:

1. **instrument `Env`** in the census (`ENV_MADE`/`ENV_LIVE` already exist and are
   dead) — one-line change that turns the biggest suspect from invisible to
   counted;
2. attrset maps + interned strings — `attrs_live` 577 K and `nixstr_live` 795 K
   are counted but their per-entry cost is not;
3. rowan green trees for every parsed file, retained via `IMPORT_CACHE` (§V.32);
4. `Rc` cycles (§V.26) — now the LAST suspect, not the first.

**The methodological point, stated because it cost two entries:** §V.26 read a
high live-COUNT as high live-BYTES without checking `size_of`. A count is not a
footprint. The `size_of` check took under a minute and refuted a hypothesis that
had already been written up twice.

## §V.34 `Env` instrumented and EXONERATED — the bytes are in the HAMT backing the value graph

Wired the dead `ENV_MADE`/`ENV_LIVE` counters (§V.33 item 1). On `minimal`:

```
env_made 6_891_927   env_live 387_866      ->  94 % FREED
thunk_live 8_078_642 (70 % of made)   attrs_live 577_344 (11 %)
list_live    799_420 (41 %)           nixstr_live 795_394 (25 %)
rss 10 362 MB
```

`Env` is created ~6.9 M times and collected almost entirely. It is **not** the
retainer — which retires §V.33's top-ranked suspect on its first measurement, the
whole point of instrumenting it.

**Where the bytes actually are.** Thunk headers are 8.08 M x 72 B = 583 MB (§V.33),
so ~94 % of the 10.36 GB is in the *contents* of the retained value graph: 577 K
attrsets, 799 K lists, 795 K strings — and, critically, the **interior nodes of the
`im_rc` HAMTs backing them, which the census does not count at all**.

`sui-eval/src/value.rs:25` — `pub type FxHashMap<K, V> = im_rc::HashMap<K, V, FxBuildHasher>`
— is used for BOTH attrset contents and env bindings. A HAMT buys the O(1)
structural-sharing `Env::child()` the evaluator depends on, but it costs several
`Rc`'d interior nodes per map, and nixpkgs' attrsets are overwhelmingly SMALL.
CppNix's `Bindings` is a flat sorted array with one allocation and no interior
nodes. That difference, multiplied across ~577 K live attrsets and every env,
is a far better fit for a 12.3x constant factor than anything measured so far.

**Correction to §V.26 and §V.32, both now retired:** the retained thunks are not
the memory (6 %), and `IMPORT_CACHE` roots the graph but does not explain its
SIZE. Two hypotheses written up before checking `size_of` and before counting the
one structure that was uninstrumented.

**Next, and it is a measurement not a redesign:** count `im_rc` interior nodes (or
compare `NixAttrs` memory against a flat-map prototype on a small attrset corpus).
If a HAMT costs ~10x a flat array for the 1-5-entry attrsets that dominate
nixpkgs, the fix is a representation split — flat for small/immutable attrsets,
HAMT only where structural sharing is actually exercised — and NOT a garbage
collector.

## §V.35 RETRACTED — the HAMT is NOT used for attrsets (see §V.36)

The bounded experiment §V.34 asked for, run rather than left as a
recommendation (`measure_hamt_vs_flat_attrset_cost`, `#[ignore]`d as a
measurement rather than a gate):

```
300 000 attrsets x 4 entries  (the size that dominates nixpkgs)
  im_rc HAMT :  423 624 704 B total   1 412 B/map
  std flat   :   82 411 520 B total     274 B/map
  ratio      :  5.14x
```

`sui-eval/src/value.rs:25` aliases `FxHashMap` to `im_rc::HashMap` and uses it
for BOTH attrset contents and env bindings. At 577 K live attrsets that is
~0.81 GB of pure HAMT overhead over a flat representation for attrsets ALONE,
before env bindings, and the multiplier compounds because every interior node is
a separate `Rc` allocation.

**This is a sufficient explanation for the 12.3x.** CppNix's `Bindings` is a flat
sorted array — one allocation, no interior nodes, no per-node refcount. sui pays
5.1x per map for a structural-sharing property that `Env::child()` genuinely
needs but that an ordinary attrset **never exercises**: an attrset literal is
built once and never structurally extended.

**The fix is therefore a representation split, not a garbage collector:**
flat storage for attrset contents (built once, read many), HAMT retained only
for `Env` bindings where `child()` actually shares. That is a bounded change to
one type's backing store, behind the existing `NixAttrs` API, and it is testable
against the same `minimal` baseline (10.85 GB) and the R2 parity gate that is
already green.

**Four of this session's own hypotheses were retired by measurement to get
here** — retained thunks are the memory (no: 6 %), `IMPORT_CACHE` explains the
size (no: it roots, does not inflate), `Env` is the retainer (no: 94 % freed),
`Rc` cycles are the cause (unproven, and now unnecessary as an explanation).
Each was written up before the cheap measurement that refuted it. The cheap
measurements — `size_of`, wiring two dead counters, this 40-line benchmark —
each took under a minute and each moved the answer.

## §V.36 §V.35 was WRONG: attrsets are ALREADY flat. The 5.14x applies only to `Env`.

Retracting §V.35, which this session committed as "CONFIRMED". The benchmark
numbers are real; the attribution was not.

```rust
value.rs:25  pub type FxHashMap<K,V> = im_rc::HashMap<K,V,FxBuildHasher>;          // Env.bindings
value.rs:41  pub type AttrsMap<K,V>  = std::collections::HashMap<K,V,FxBuildHasher>; // NixAttrs
```

`NixAttrs` stores `AttrsMap`, i.e. a **flat `std::HashMap`**. Attrsets never pay
the HAMT cost, so "577 K live attrsets x HAMT overhead" — the whole basis of
§V.35's conclusion — describes something that does not exist. The persistent map
is confined to `Env.bindings`, where `child()` genuinely needs the sharing.

**What the 5.14x actually bounds.** `env_live` is 387 866 (§V.34), so even at
1 412 B/map the live HAMTs are ~0.55 GB of a 10.36 GB peak. Real, worth having,
nowhere near sufficient.

**Where the bytes are NOT.** Summing everything the census counts, with measured
per-object sizes:

```
thunks   8 078 642 x  72 B  =  0.58 GB
envs       387 866 x 1412 B =  0.55 GB
attrsets   577 344 x ~274 B =  0.16 GB
                              -------
                               ~1.3 GB  of a 10.36 GB peak
```

**~9 GB is in structures the census does not count at all.** The leading
candidate is now the one §V.32 raised and §V.35 displaced: the **rowan green
trees** for every parsed nixpkgs file, retained for the process lifetime by
`IMPORT_CACHE` plus every unforced `Suspended { expr, .. }` thunk. `rnix::ast::Expr`
is 16 B (§V.33) because it is a *handle*; the green tree it points into is not,
and nothing counts it.

**Next measurement, and it is again cheap:** count `IMPORT_CACHE` entries and sum
the byte length of the source text each one parsed, then compare against the
unaccounted ~9 GB. If nixpkgs' parsed ASTs dominate, the fix is parse-tree
retention (drop green trees once a file's value is fully forced), NOT a map
representation and NOT a collector.

**Fifth hypothesis retired, and the first one I had already published as
confirmed.** The failure was reading `FxHashMap` at line 25 and assuming it was
the map `NixAttrs` uses, without checking line 41 sixteen lines below. The
benchmark then "confirmed" a cost that the code does not pay — a measurement
answering a question the code had already settled differently.

## §V.37 Source retention measured (34.4 MB, 3 056 files) — and a thread-local trap caught in the act

Instrumented `SOURCE_TEXTS` per §V.36. First reading: `src_files=0
src_bytes=0.0MB` for an evaluation that had demonstrably parsed thousands of
files. **That was the harness, not the finding.** `SOURCE_TEXTS` is a
`thread_local` and the census exit dump runs on the periodic-dump THREAD, where
the map is empty — a cross-thread read of thread-local state, whose output is
indistinguishable from "nothing was ever registered". The other counters read
correctly only because they are global atomics. Re-backed by atomics:

```
src_files=3056   src_bytes=34.4MB
```

**So the text registry is negligible**, and at a typical ~20x rowan green-tree
expansion the parse trees are on the order of 0.7 GB — real, but not the answer
either.

### The accounting, with every measured number this session

```
thunks     8 078 642 x   72 B  = 0.58 GB   (§V.33)
envs         387 866 x 1412 B  = 0.55 GB   (§V.34, §V.36)
attrsets     577 344 x  274 B  = 0.16 GB   (flat std::HashMap, §V.36)
source text  3 056 files       = 0.03 GB   (here)
green trees  ~20x source       ≈ 0.7  GB   (ESTIMATED, not measured)
                                 -------
                                 ~2   GB   of a 10.36 GB peak
```

**~8 GB remains unaccounted**, and every structure the census knows about has now
been counted. Closing the gap needs heap-ownership attribution — a real profiler
— which is unavailable in this sandbox (`ps -o rss`, `ps -o cputime` and
`log show` for jetsam all fail with `requires entitlement`). Further hypothesising
from structure would repeat what §V.26/§V.32/§V.35 each did: name a plausible
retainer, then find it accounts for a few percent.

**What is now solid about the memory blocker:** it is 12.3x CppNix, measured on a
config that completes; it is not thunks, not `Env`, not attrset representation,
not the import-source registry, and not `Rc` cycles by any evidence collected.
The next honest step is a profiler on a machine without these restrictions, not
another structural guess.

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
