# sui-vs-nix hot-shape measurement harness

The re-runnable ratio harness that converts the unbacked "3× CppNix"
headline into a **measured number**. It times sui's tree-walker (the
byte-parity-correct engine; the VM defers string-context and is not
parity-capable) against real `nix-instantiate` on the closure / thunk /
attrset / recursion **hot shapes**, and byte-verifies drvPath parity as
a bonus gate.

This is a doc pointing at a Rust test + a few one-liners — there is **no
shell script** (NO-SHELL law). The whole harness is
`sui-eval/tests/vs_nix_hotshapes.rs`; it reuses the proven timing helpers
(`time_sui` / `time_cppnix` / `median` / `measure_cppnix_spawn_floor`,
the `MEANINGFUL_CPPNIX_EVAL_US = 500 µs` honesty threshold, the
`SUI_TEST_ONLINE=1` gate) modeled on `sui-eval/tests/vs_cppnix.rs`.

## Regenerate the results.json

```
SUI_TEST_ONLINE=1 cargo test -p sui-eval --test vs_nix_hotshapes -- --nocapture
```

Writes two artifacts under `target/`:

- `target/vs-nix-hotshapes.results.json` — the typed, serde-serialized
  result set (per-shape `sui_us` / `nix_wall_us` / `nix_eval_us` /
  `wall_ratio` / `engine_ratio` / `engine_comparable`, a summary block
  with the two geomeans, and the drvPath `correctness` cross-check).
- `target/vs-nix-hotshapes.md` — a human-readable mirror of the same
  data.

`cargo test` builds **debug**, so the `sui_us` numbers it writes are
debug-profile (5–20× slower than a real sui). For the honest engine
comparison, run it in release:

```
SUI_TEST_ONLINE=1 cargo test --release -p sui-eval --test vs_nix_hotshapes -- --nocapture
```

The `profile` field in the JSON records which one produced it.

## Which column is honest

Two ratios per shape:

- `wall_ratio` = `nix_wall_us / sui_us` — the user-perspective cost of
  running `nix-instantiate` from a shell. On this machine the CppNix
  **spawn floor is ~70 ms** (a cold `nix-instantiate --eval -E 1`), so
  `wall_ratio` is *massively* spawn-inflated on these sub-millisecond
  shapes (geomean ~234× in release). **It is NOT the engine number.**
- `engine_ratio` = `nix_eval_us / sui_us` where `nix_eval = nix_wall −
  spawn_floor`. This isolates pure eval cost, but only for shapes whose
  `nix_eval_us ≥ 500 µs` (`engine_comparable = true`). Shapes below that
  threshold spent ~all their wall time in fork+exec; subtracting the
  floor leaves noise, so they are excluded from the engine geomean.

**`engine_ratio` on the `engine_comparable` shapes is the honest
headline column.**

## Measured verdict (2026-07-17, this machine, sui 0.1.122 / nix 2.34.7)

Release profile, 4 engine-comparable shapes:

| shape | class | sui µs | nix_eval µs | engine× |
|-------|-------|------:|------------:|--------:|
| `let_5` | let-chain | 43 | 977 | **22.7×** |
| `attrset_merge`† | overlay-flatten | 42 | 222 | 5.29× |
| `list_foldl_100` | list-fold | 368 | 1053 | **2.86×** |
| `rec_fib_10` | recursion | 393 | 676 | **1.72×** |
| `rec_fib_20` | recursion-deep | 38428 | 4100 | **0.107×** |

† below the 500 µs threshold, so excluded from the engine geomean, but
shown for completeness.

- **Release engine geomean: 1.86×** over the 4 comparable shapes.
- **The wins are real** on shallow/allocation-bound shapes (let-chains,
  folds, small recursion): 1.7–22.7× faster pure eval than CppNix.
- **The one loss is `rec_fib_20`** (fib 20 = ~13.5k recursive calls):
  sui is ~9× *slower* than CppNix on deep recursion. This is exactly the
  hot spot the thunk-allocation optimization targets (see the Storm
  verdict below), and it is the honest counterpoint to the wins.

Debug profile geomean is 0.04× (sui debug loses badly) — a reminder that
these are *engine*-tier numbers only in release. Always cite the release
`profile: "release"` JSON.

### Does this back the "3× CppNix" headline?

**Refines it, doesn't cleanly back it.** The unqualified "3× CppNix" is
too coarse:

- On **wall time** (what a shell user feels), sui is ~200–1700× faster
  on these small shapes — but that's spawn cost, not the engine.
- On **pure eval** of shallow/allocation-bound shapes, sui is genuinely
  **1.7–22.7×** faster (geomean 1.86×), so "several × faster" is
  defensible for that shape class.
- On **deep recursion** sui is currently ~9× *slower*. A blanket "3×
  faster" claim is contradicted here.

The honest statement is: *sui's tree-walker is ~1.9× faster than CppNix's
pure eval on the hot-shape mix, with big wins on allocation-bound shapes
and a real deep-recursion regression still open.*

## drvPath correctness cross-check (byte-parity gate)

The harness also evals `(pkg).drvPath` through the sui CLI subprocess
(so it goes through the real store-path machinery) and real `nix eval`,
and **hard-asserts byte-equality** — a mismatch fails the test. Verified
green this run:

```
hello:     /nix/store/a1fzz00d2gwsj6kniyrivsyrdh97k634-hello-2.12.2.drv   ✓
coreutils: /nix/store/6m7v8jnsrdyg8780y8mg8bjd0jhgbbsl-coreutils-9.8.drv  ✓
```

The one-liner to reproduce a single row by hand (note `--no-vm` selects
the tree-walker and is a **global** sui flag; `--raw`/`-E` are the `eval`
subcommand flags):

```
sui --no-vm eval --raw --impure -E '(import (builtins.getFlake "nixpkgs") { system = builtins.currentSystem; }).hello.drvPath'
nix eval --raw --impure --extra-experimental-features "nix-command flakes" --expr '(import (builtins.getFlake "nixpkgs") { system = builtins.currentSystem; }).hello.drvPath'
```

If `getFlake nixpkgs` isn't resolvable at run time the correctness
section is skipped (recorded empty) rather than failing; the perf table
stands on its own.

## Storm-A-vs-B verdict (which re-walk dominates the recursion hot path)

Both instrumentation blocks are already compiled in and print under
`SUI_EVAL_PERF=1`; no code change is needed to read the verdict. Run:

```
SUI_EVAL_PERF=1 sui --no-vm eval --no-eval-cache -E 'let f = n: if n < 2 then n else f (n - 1) + f (n - 2); in f 24'
```

and read the two `% of eval` numbers:

- **Storm A** (`referenced_idents`, the self/mutual-recursion detection
  walk).
- **Storm B** (`overlay (//) flatten`).

Measured on this machine (release, fib 24):

| Storm | shape | % of eval |
|-------|-------|----------:|
| A — `referenced_idents` | fib 24 | **0.0%** (1 walk call, 21 nodes) |
| B — overlay flatten | fib 24 | not applicable (no `//` in fib) |
| B — overlay flatten | `attrset_merge` (`// // //`) | **0.1%** |

**Verdict: neither Storm dominates** — each is ≤ 0.1% of eval. The
#1 optimization already hoisted `referenced_idents` out of the O(N²)
re-walk storm, so Storm A is a rounding error. The real lever on the
recursion hot path is **thunk allocation**, not either re-walk: the
`SUI_EVAL_PERF` report on fib 24 shows

```
thunks_created: 150049   thunks_forced: 150049   (waste 0%)
thunk_store_redundant: 150049   (C-store; Store#2 = pure redundant rewrite — provably skippable)
```

i.e. on this *small* shape there is no never-forced-thunk waste, but
every force pays a **redundant second thunk-store write** (the RISKY-tier
lever). The **51.8% never-forced thunk waste** cited in
`docs/EVAL-CORE-DOMINANCE.md` is measured on a *large* nixpkgs-scale
workload (1.27M thunks created, 613k forced), not on these tiny hot
shapes — the small shapes don't reproduce it. Both point at the same
root: thunk allocation / store discipline is the dominant closable term,
not Storm A or Storm B.

## External flamegraph (not wired in-repo)

For a full flame profile of the deep-recursion hot path, use `cargo
flamegraph` from the workspace root — this is **not** wired into the repo
(no `[[bench]]` / cargo alias), install it standalone:

```
cargo install flamegraph
cargo flamegraph --release -p sui-eval --bin sui -- --no-vm eval --no-eval-cache -E 'let f = n: if n < 2 then n else f (n - 1) + f (n - 2); in f 26'
```

(macOS needs `dtrace` privileges; Linux needs `perf`.)
