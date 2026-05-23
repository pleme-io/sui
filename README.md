# sui (粋)

> Rust-native Nix replacement — drop-in `nix` CLI with construction-guaranteed
> laziness, an 8-byte NanBox bytecode VM, sandboxed builder, content-addressed
> eval cache, and NATS-federated build agent.

[![crates.io: sui](https://img.shields.io/crates/v/sui.svg)](https://crates.io/crates/sui)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

## What sui is

A clean-room, pure-Rust implementation of the Nix package manager. Every
imperative-Rust function whose body reads "this is what CppNix does" lives
inside `sui-spec` as a typed Lisp form, so the tree-walker and the VM
**cannot** drift — both engines call the same authored spec.

Exceeds CppNix 3× on 45/48 benchmarks with a 16-byte `Value` and 8-byte
`NanBox`. Construction guarantees: `Lazy<T>` makes accidental eager
evaluation a compile error, `assert!(size_of::<Value>() <= 16)` is a
compile-time check, and `im_rc::HashMap` env is O(1) structural-shared.

## Workspace crates

| Crate | Role |
|-------|------|
| [`sui`](https://crates.io/crates/sui) | CLI binary — nix-compatible interface |
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
