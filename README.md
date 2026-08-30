# sui (粋)

> Rust-native Nix replacement — drop-in `nix` CLI with construction-guaranteed
> laziness, an 8-byte NanBox bytecode VM, sandboxed builder, content-addressed
> eval cache, and NATS-federated build agent.

[![crates.io: pleme-io-sui](https://img.shields.io/crates/v/pleme-io-sui.svg)](https://crates.io/crates/pleme-io-sui)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

## Install

```
cargo install pleme-io-sui     # installs a binary named `sui`
```

Or with Nix, which is how the fleet consumes it:

```
nix run github:pleme-io/sui -- --version
```

> **The crates.io package is `pleme-io-sui`, not `sui`.** The name `sui`
> on crates.io is a reservation held by MystenLabs for the Sui
> blockchain, so **`cargo install sui` installs something unrelated to
> this project.** Only the registry identity differs — the library is
> `use sui::…` and the installed binary is `sui`. The sub-crates
> (`sui-eval`, `sui-bytecode`, …) are unaffected and publish under their
> own names.

## What sui is

A clean-room, pure-Rust implementation of the Nix package manager. Every
imperative-Rust function whose body reads "this is what CppNix does" lives
inside `sui-spec` as a typed Lisp form, so the tree-walker and the VM
**cannot** drift — both engines call the same authored spec.

**Performance, measured — and the honest version is mixed.** The only
real sui-vs-nix harness (`vs_nix_hotshapes.rs`) reports a **1.86× engine
geomean** and a **9× LOSS on `rec_fib_20`**, the deep-recursion shape a
system-rebuild evaluation hammers. Whole-system evaluation
(`nix eval .#darwinConfigurations.cid.system.drvPath`) **has never
completed** in sui; CppNix does it in 107s. Memory runs ~22GB against
nix's 10.68GB — representation overhead, not a leak.

There is also a **bifurcation** worth knowing before you benchmark
anything: the fast bytecode VM **cannot** produce the marquee numbers and
yields **wrong drvPaths** (it bridges per-file to the tree-walker on
nixpkgs import and defers string-context), so the parity-correct engine
is the slower `--no-vm` tree-walker. Speed and correctness are currently
different binaries. Full accounting, including what each number was
measured on, is in
[`docs/SUI-EQUIVALENCE.md`](docs/SUI-EQUIVALENCE.md).

> Corrected 2026-08-30. This paragraph previously read *"Exceeds CppNix 3×
> on 45/48 benchmarks"*. That figure had no harness behind it and was
> already contradicted by this repo's own equivalence doc — it was prose
> in two files, not a measurement.

Construction guarantees: `Lazy<T>` makes accidental eager evaluation a
compile error, `assert!(size_of::<Value>() <= 16)` is a compile-time
check, and `im_rc::HashMap` env is O(1) structural-shared.

## Workspace crates

| Crate | Role |
|-------|------|
| [`pleme-io-sui`](https://crates.io/crates/pleme-io-sui) | CLI binary (installs as `sui`) — nix-compatible interface |
| [`sui-eval`](https://crates.io/crates/sui-eval) | Tree-walker evaluator + `Lazy<T>` primitives |
| [`sui-bytecode`](https://crates.io/crates/sui-bytecode) | Bytecode VM (NanBox 8B, 44+ opcodes, TAILCALL) |
| [`sui-intern`](https://crates.io/crates/sui-intern) | String interning (Symbol u32, thread-local Interner) |
| [`sui-cache-eval`](https://crates.io/crates/sui-cache-eval) | Content-addressed eval cache (BLAKE3 keys) |
| [`sui-compat`](https://crates.io/crates/sui-compat) | Nix formats (NAR, store paths, ATerm, derivations) |
| [`sui-store`](https://crates.io/crates/sui-store) | Store abstraction (SeaORM / SQLite) |
| [`sui-build`](https://crates.io/crates/sui-build) | Build execution (sandboxed builder) |
| [`sui-cache`](https://crates.io/crates/sui-cache) | Binary cache (S3, local, redb) |
| [`sui-daemon`](https://crates.io/crates/sui-daemon) | Daemon mode (worker protocol) |
| [`sui-orchestrate`](https://crates.io/crates/sui-orchestrate) | System rebuild + fleet deployment |
| [`sui-spec`](https://crates.io/crates/sui-spec) | Declarative Lisp-authored CppNix-parity specs + the shadow-rebuild substrate |

## Try it

```sh
# install from crates.io
cargo install sui

# evaluate a Nix expression
sui eval --json '1 + 2'

# show a flake's outputs
sui flake show github:NixOS/nixpkgs

# build something
sui build .#default
```

## Differential testing

`sui-spec` ships the shadow-rebuild substrate: a typed [`ParityCheck`] trait,
a typed dual-subprocess runner (`exec::dual_run`, NO SHELL), and three
canonical Lisp probe corpora that run sui side-by-side against cppnix as
the oracle.

```sh
# install the differential runner
cargo install sui-spec

# sweep every pleme-io flake in ~/code/github/pleme-io with all corpora,
# write a typed JSON report to ~/.cache/sui/shadow-reports/<host>-<ts>.json
sui-sweep

# only run the builtin-module smoke corpus (fast, hermetic)
sui-sweep --corpus builtins

# only run the rebuild-stage probes against the current host
sui-sweep --corpus rebuild --tag rebuild-phase-1
```

The rebuild corpus targets every stage of a real `nixos-rebuild` /
`darwin-rebuild`: flake show, flake check, per-input lock-hash parity,
toplevel eval, home-manager activation, dry-run closure, closure size,
reference-graph. Sui shadows a real rebuild without ever mutating the
system.

## Architecture

`docs/`, the inline CLAUDE.md, and per-crate rustdoc are authoritative.
High-level: Source → `rnix::parse` → AST → either tree-walker (lazy
thunks, HAMT env) or bytecode VM (8B NanBox, slot locals, TAILCALL).
Both engines call the same `sui-spec` interpreters, so derivation
hashing, flake result shape, and parity probes are drift-free by
construction.

## Status

- Eval + flake + derivation + build: **production-ready** for clean-room
  evaluation of Nix language + flakes.
- System rebuild (`darwin-rebuild` / `nixos-rebuild` parity): **shadow-testing
  surface in place**; per-stage sui-primary path lands as the module-system
  module-system lattice is completed.
- CA-derivations + the full Nix module system: in progress.

## License

MIT — see [LICENSE](LICENSE).

[`ParityCheck`]: https://docs.rs/sui-spec/latest/sui_spec/parity/trait.ParityCheck.html
