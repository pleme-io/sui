# Silent-divergence hunting — the method, and what it found

> **Read this before you next try to close the nix→sui flip.** It is not a
> status page. It is the *instrument*: how to find the class of bug that
> `sui parity` structurally cannot see, why that class is the one blocking the
> flip, and how to make finding it deterministic instead of lucky.
>
> **Dated 2026-08-09 against `sui 0.1.171`.** Every number below was measured
> on cid (aarch64-darwin) against `nix 2.34.7`, with the command shown.
> Re-measure before acting; do not cite a figure from here without re-running
> it. That rule exists because this document was written *because* the
> repo's own `CLAUDE.md` had gone stale on its most load-bearing claim.

---

## I. The situation, in one paragraph

sui's evaluator is far stronger than its own documentation says, and the
remaining distance is almost entirely a class of bug the enforcing gate is
shaped so it cannot detect. In one session, evaluating the operator's real
fleet flake found **two silent divergences** — both of which produced valid,
plausible, *wrong* values with no error anywhere. One renamed every NixOS
system on the fleet. Neither would ever have been caught by the corpus.

---

## II. What is actually true today (measured, with commands)

| what | result | command |
|---|---|---|
| `sui parity` corpus | **77/77 sealed, 0 regressions** | `sui parity` |
| real nixpkgs drvPaths | **10/10 byte-identical** | `sui eval --impure --raw --expr '(import <nixpkgs> {}).<pkg>.drvPath'` |
| flake w/ `github:` input + lock | **byte-identical drvPath** | trivial flake, `sui eval --raw .#…drvPath` |
| fleet gate (781 assertions) | **29 = nix** *(was 776/781)* | `sui eval .#kataFleetGate` in `pleme-io/nix` |
| NixOS toplevel eval | **completes, and the hash MATCHES nix** (2026-08-17; was "hash still differs" — see §VI.1) | `sui eval --no-eval-cache --raw .#nixosConfigurations.minimal…drvPath` |
| `sui-eval` suite | 1377 pass / 11 fail (pre-existing) | `cargo test --release -p sui-eval` |

**Two corrections to `CLAUDE.md`, which is stale on both.** It says the parity
gate is *"inert on CI — 0 successes in the last 40 runs"* and that *"releases
have cut through the red"*. The corpus is sealed at 77/77, and
`.github/workflows/parity.yml` already implements the destination that doc
describes as pending (`SUI_PARITY_PUREONLY=1` skips unevaluable rows; a
wrong byte still fails). Fix the doc before you plan against it.

---

## III. The two bugs, because their *shape* is the lesson

### III.1 `.` did not match a newline

```
builtins.match ".*b.*" "a\nb"    nix: [ ]    sui: null
builtins.match ".*b.*" "ab"      both match
```

CppNix compiles with `std::regex::extended` (POSIX ERE) where `.` matches any
character **including `\n`**. Rust's `regex` crate defaults the other way and
sui inherited it.

**Why it mattered far beyond regexes:** nixpkgs implements `lib.hasInfix` as
`builtins.match ".*<needle>.*"`. So *any* `hasInfix` over generated
**multi-line** text — a script body, an ssh config block, an activation
snippet, a wrapper — silently returned `false`. Five wrong assertions in the
fleet's own suite. No error.

Fixed: `RegexBuilder::…dot_matches_new_line(true)` in
`sui-eval/src/builtins/strings.rs` (+ `sui_ext.rs` for consistency).

### III.2 `lastModifiedDate` was absent

```
sui …inputs.nixpkgs.lastModified      1782847189        correct
sui …inputs.nixpkgs.lastModifiedDate  attribute not found
```

nixpkgs builds `versionSuffix` from `lastModifiedDate`. Absent → epoch →

```
nix:  nixos-system-minimal-25.11.20260630.b6018f8.drv
sui:  nixos-system-minimal-25.11.19700101.b6018f8.drv
```

The system **name** is an input to its own drvPath. **One missing attribute
renamed every system on the fleet**, and it evaluated cleanly.

Fixed at all four emission sites in `builtins/flake_eval.rs`.

### III.3 The common shape — and it is the whole point

Neither bug was a crash, a type error, or a refusal. Both produced a
**plausible wrong value**. Both were in code paths the 77-row corpus exercises
happily. Both were found only by evaluating something *real* and comparing
against nix.

> `CLAUDE.md`'s north star already says this: *silent divergence — a wrong
> value that still evaluates — is the worst failure of all, worse than a
> crash.* What was missing was an instrument that finds it.

---

## IV. ★ The core problem, stated precisely

**The corpus is shaped so that it cannot see the failure classes the flip
depends on.**

The corpus makes four choices:

1. `--no-vm` (tree-walker), while `sui eval` **defaults to the bytecode VM**
2. `--impure`
3. `path:` flakerefs
4. one floating `<nixpkgs>` as oracle

**A rebuild is a pure flake eval of a git working tree on the default
engine.** That is the complement of all four choices at once — and it is the
only call the flip actually requires.

This is not bad luck. It is the shape a corpus takes when it is grown by
closing observed reds: each row is added where a divergence was *already*
noticed, so the corpus converges onto the region already known to be safe.
A green corpus therefore measures the absence of *known* bugs, never the
presence of correctness.

---

## V. ★★ The deterministic fix — three mechanisms, in order

### V.1 Make the differential run on REAL targets, not synthetic rows

The corpus should include, as first-class rows, the artifacts the fleet
actually builds:

```
nixosConfigurations.<node>.config.system.build.toplevel.drvPath
darwinConfigurations.<host>.system.drvPath
homeConfigurations.<user>.activationPackage.drvPath
```

for at least one node per class. Both bugs above were on this path and on no
other. A row is cheap: it is one `sui eval` + one `nix eval` + a string
compare. Start with `nixosConfigurations.minimal` — it completes in seconds
and already exposes a live divergence today (§VI).

**The invariant:** *the corpus must contain the call the flip requires.*
Anything else proves a neighbourhood of it.

### V.2 Key the eval cache on the evaluator — ✅ **DONE 2026-08-09**

> **Fixed and proven end-to-end.** With the cache ON, sui now returns the
> corrected value where it previously served the pre-fix one. Landed at
> **two** sites, because there are two key functions and fixing one is not
> enough — see the note at the end of this section.
>
> **Residual, deliberately left:** the key uses the crate VERSION, so two
> builds of the SAME version with different code — i.e. local evaluator
> work — still share entries. That covers every PUBLISHED sui, which is the
> case that reaches other machines. **While working ON the evaluator, use
> `--no-eval-cache`.** Closing it fully needs a build identity (git rev or
> source hash) stamped by `build.rs`, which does not exist today.

The problem it solved:

```
sui eval …toplevel.drvPath                  …19700101…   (stale, OLD binary)
sui eval --no-eval-cache …toplevel.drvPath  …20260630…   (correct, new binary)
```

**A cache entry written by an old sui is served by a new one.** Consequences,
in ascending severity:

1. Fixing a divergence does not fix it for any machine with a warm cache.
2. Every silent divergence becomes a *durable* one.
3. **You cannot verify your own fix.** The first post-build check of §III.2
   returned the epoch name and read as *"the patch did nothing"*.

**THE TRAP THAT COST TWO FAILED ATTEMPTS — there are TWO key functions.**
`EvalCache::key_for_file` (sui-eval/src/eval_cache.rs) keys file-based evals,
while the CLI's `.#installable` path uses `eval_cache_key_for_installable`
(src/main.rs), which builds a `CacheKey` independently. Fixing only
`graph_hash_for_key` misses the memory and JSON tiers, which key on
`CacheKey` DIRECTLY; fixing only `key_for_file` misses the CLI path entirely.
Both attempts looked correct, passed their unit tests, and changed nothing
end-to-end. **Any future key-scoping change must touch both, and must be
proven with a real `sui eval` — not a unit test.**

### V.3 Compare SCALARS, never renderings

sui's value printer differs from nix's in two ways that manufacture phantom
divergences:

- it does **not deep-force for display** — `[ <<thunk>> ]` where nix shows
  `[ "A" ]`;
- it prints **literal newlines and unescaped quotes** inside strings where
  nix escapes them — so any `| tail -1` in a comparison harness truncates a
  multi-line value and invents a dropped element.

Both fooled the author of this document, twice. `lib.mapAttrsToList` was
accused of returning broken thunks on that evidence; it is **fine**, as
`elemAt` / `stringLength` / `deepSeq` immediately show.

**The rule:** compare `builtins.stringLength`, `typeOf`, `==`, a boolean, or
`--raw`. Never compare printed output. The moment a printer sits between you
and the value, a display artifact is indistinguishable from a real
divergence — the same "failure mode identical to success mode" property that
makes a tool useless for verification.

Switching to scalars-only is what isolated §III.1 in a single step:
`stringLength` **97 on both sides** while `hasInfix` disagreed points at the
regex, not at the string.

> That printer difference is *also* a real CLI-parity gap in its own right —
> `nix eval` output is an interface people script against — and deserves its
> own fix.

---

## VI. The next divergence, already located for you

After §III.2, the system **name** matches and the **hash does not**:

```
nix:  /nix/store/r2f7b3hlx9b5hvm10ai6pyd7klrr26hx-nixos-system-minimal-25.11.20260630.b6018f8.drv
sui:  /nix/store/009ac2i4pw5yj82zg45avvw6qz3n6687-nixos-system-minimal-25.11.20260630.b6018f8.drv
```

Same name, different content. At least one more divergence lives inside that
derivation.

**Corrected 2026-08-18 — the trap recorded here was wrong in both halves.**

This said `sui eval …drvPath` does **not write the .drv** and that you must
instantiate first. Measured: it DOES write it, during eval
(`write_derivation_to_store`, called from the `derivation` builtin). It just
does not always land in `/nix/store` — that directory is `root:nixbld` and the
operator is not in `nixbld`, so on `PermissionDenied` it falls back to
`$TMPDIR/sui-drv-cache/<basename>`. Both `read_drv_bytes` sites already apply
that same fallback rule, so the two sides are diffable after a plain eval.
There is no `sui instantiate` subcommand and none is needed.

One real caveat survives: when sui's path EQUALS an existing nix store path the
write is skipped (`if !drv_file.exists()`), so nothing new appears — but the
file is readable at `/nix/store` anyway. Divergent paths always get cached.

**The visit-all walk this paragraph asked for LANDED 2026-08-18.**
`bisect_drv_all` visits every diverging node with a pair seen-set and a
path-keyed `parsed` memo. It fixed a second false negative nobody had named:
inputs were paired with `BTreeMap<name, path>`, so a repeated name kept only
the last path — and `source.drv` occurs 188 times in the NixOS minimal
toplevel, with the toplevel's own immediate `inputDrvs` carrying duplicates.
Nodes were being dropped at the root of this very subject.

---

## ★ VI.1 — CLOSED, 2026-08-17. Re-measure before believing anything above.

**The divergence in §VI no longer exists.** Same subject
(`nixosConfigurations.minimal` at `pleme-io/nix` `parts/nixos.nix:47`), same
nixpkgs pin (the drv name still ends `25.11.20260630.b6018f8`), measured
independently by two agents and re-run by hand:

```
nix:  /nix/store/xqc77nw99kzdqnqly2lfjmm8mlljk5sp-nixos-system-minimal-25.11.20260630.b6018f8.drv
sui:  /nix/store/xqc77nw99kzdqnqly2lfjmm8mlljk5sp-nixos-system-minimal-25.11.20260630.b6018f8.drv
```

sui produces **neither** of the two hashes recorded above — it produces nix's.
Run with `--no-eval-cache` on both sides, on the tree-walker (the default
engine since 2026-08-17), `sui` 0.1.202 release.

**What is NOT claimed:** that any specific commit closed it. The flake tree
moved between 2026-08-09 and today, and no bisect was run. What is proven is
that *today*, on *this* tree, the two agree.

### VI.2 — the cost, measured on the release binary

| | sui 0.1.202 release | nix 2.31.5 | ratio |
|---|---|---|---|
| peak RSS | **9.99 GB** (two runs, identical) | 0.91 GB | **11.0×** |
| wall | 19.1 s | 4.99 s | 3.8× |

`nix-store -q --requisites` on that drv → **3,489 nodes**, so sui costs
**2.94 MB per closure node**.

**★ The debug→release constant is not 2.3×, and it is not one constant.**
A first pass measured only a DEBUG build (263 s / 8.94 GB) and deflated it by a
factor taken from a doc, concluding release ≈ 4.2 GB here and ≈ 23 GB at cid
scale — which would FIT in this machine's 48 GiB and therefore contradict the
MemoryWall verdict it was offered in support of. Measured properly:

* **wall: ~14× faster in release** (263 s → 19.1 s)
* **memory: 1.12× WORSE in release** (8.94 GB → 9.99 GB)

Optimisation buys time, not footprint, and here it costs footprint. Do not
deflate a memory figure by a speed ratio.

### VI.3 — the MemoryWall verdict stands, for the opposite reason

At 2.94 MB/node, cid's **18,995** requisites (re-measured today; `STRATOSPHERE`
says 20,827) extrapolate to **≈ 56 GB** against 48 GiB of RAM. So
`STRATOSPHERE.md:300`'s `CANNOT-COMPLETE at cid scale (memory wall,
swap-death)` is **still correct** — but the arithmetic offered for it pointed
the other way, and a reader who checked only the conclusion would have banked a
right answer resting on a wrong model.

`STRATOSPHERE.md:356` is now **understated**, though: it says to run the closure
walk against "a SMALL pinned closure (leaf drvPath), NOT cid". A full NixOS
toplevel at 3,489 nodes completes in 19 s and 10 GB and is byte-identical to
nix — 5.4× richer than a leaf, and the correct M5 subject.

And this file's own §II table row — *"NixOS toplevel eval **completes**, correct
name, **hash still differs**"* — is stale in its second half. Fix the row rather
than citing it.

---

## VII. The loop, for the next agent

1. **Fix the eval cache key** (§V.2). Until then no verification means anything.
2. Pick a real target — start `nixosConfigurations.minimal`, then a darwin host.
3. `sui eval --no-eval-cache --raw <target>.drvPath` vs `nix eval --raw` same.
4. Differ? Instantiate both, diff the drvs, shrink to a single expression.
5. Shrink using **scalars only** (§V.3).
6. Fix the load-bearing cause. Never a band-aid — `CLAUDE.md` is right that a
   sentinel or a `null`-so-eval-proceeds hides a divergence the oracle
   resurfaces downstream.
7. **Add the shrunk expression to the corpus as a row**, so the class cannot
   return.
8. Re-run `sui parity` + `cargo test -p sui-eval`. Compare failures against a
   **stashed baseline** — do not assume a pre-existing red is pre-existing.

---

## VIII. Honest distance

**Eval is close.** Real nixpkgs packages byte-identical, flakes byte-identical,
a system toplevel completing with the right name.

**It is not there.** That toplevel hash still differs, and the number of
remaining divergences is *unknown* — the only instrument that finds them is
§VII, and it had never been run against a real fleet target before today.

**Build has barely started.** The build-parity basket is 2 synthetic
derivations, zero nixpkgs packages, and on a multi-user store the passing row
is a **tautology**: the nix daemon performed both builds. sui has never
independently built anything and byte-compared the output NAR.

**Remote build is `NotImplemented`** — fleet-fatal from a darwin host
regardless of parity quality, since a darwin host cannot build
`x86_64-linux` natively.

**Months, not weeks.** But the shape is now clear and each divergence is
small, precise, and closable in an afternoon. Two were found and fixed in one
session by running §VII exactly once.

---

## IX. Two smaller gaps found in passing

- **`NIX_PATH=nixpkgs=flake:nixpkgs` is unsupported.** nix resolves the
  `flake:` search-path form; sui errors `search path '<nixpkgs>' not in
  NIX_PATH`. That is the modern default on any flakes-enabled machine, so
  every `<nixpkgs>` expression fails under sui — making it look broken on
  packages it gets byte-perfect. Contained fix, large optics.
- **`sui eval` defaults to the bytecode VM while every parity surface runs
  `--no-vm`.** The engine that is *proven* and the engine that *runs* are
  different programs. Both bugs here reproduce on **both** engines, so this
  did not cause them — but the split remains, and a VM-only divergence would
  currently be invisible to the corpus.
