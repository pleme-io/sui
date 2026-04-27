# Sui (粋) — Rust-Native Nix Replacement

> **★★★ CSE / Knowable Construction.** This repo operates under
> **Constructive Substrate Engineering** — canonical specification at
> [`pleme-io/theory/CONSTRUCTIVE-SUBSTRATE-ENGINEERING.md`](https://github.com/pleme-io/theory/blob/main/CONSTRUCTIVE-SUBSTRATE-ENGINEERING.md).
> The Compounding Directive (operational rules: solve once, load-bearing
> fixes only, idiom-first, models stay current, direction beats velocity)
> is in the org-level pleme-io/CLAUDE.md ★★★ section. Read both before
> non-trivial changes. Pure-Rust Nix replacement; an in-progress Layer-1
> normalization platform replacing CppNix with construction guarantees.

<!-- Blackmatter alignment: pillars 1, 9 -->
<!-- See ~/code/github/pleme-io/BLACKMATTER.md for pillar definitions. -->

## Blackmatter pillars upheld

- **Pillar 1** (Rust + tatara-lisp + WASM/WASI): Sui is the Rust half of the language stack, taken all the way down — a pure-Rust evaluator + bytecode VM + build system that replaces CppNix. Exceeds CppNix 3x on 45/48 benchmarks with a 16-byte Value and 8-byte NanBox.
- **Pillar 9** (SDLC): Sui extends the SDLC from single-node to distributed. `sui build` replaces `nix build`; NATS-triggered rebuilds shard across a cluster; Attic is the shared convergence memory. Store paths stay the content-hash proof in either mode.

Pure-Rust Nix evaluator + build system. Drop-in `nix` CLI replacement.

## Core Philosophy: Construction Guarantees

Sui's architecture makes entire categories of bugs **impossible by construction**.
Rust's type system enforces invariants at compile time — no runtime checks needed.

| Guarantee | Mechanism | What it prevents |
|-----------|-----------|-----------------|
| **Laziness** | `Lazy<T>` wrapper — `.demand()` required to access | Accidental eager evaluation |
| **Value size** | `assert!(size_of::<Value>() <= 16)` at compile time | Cache-hostile value bloat |
| **Thunk memoization** | `OnceCell` fast-path — evaluated exactly once | Redundant computation |
| **Env sharing** | `im_rc::HashMap` HAMT — O(1) structural clone | Expensive deep copies |
| **String identity** | `Symbol(u32)` interning — comparison is `==` on u32 | String allocation in hot loops |
| **Memory safety** | Zero `unsafe` in evaluator logic (19 justified blocks in value.rs) | Use-after-free, data races |

**The pattern:** encode the invariant in the TYPE. The compiler enforces it. Bad states are unrepresentable.

## Workspace (11 crates)

| Crate | Purpose |
|-------|---------|
| `sui` (root) | CLI binary — nix-compatible interface |
| `sui-eval` | Tree-walker evaluator + `Lazy<T>` primitives |
| `sui-bytecode` | Bytecode VM (NanBox 8B, 44+ opcodes, TAILCALL) |
| `sui-intern` | String interning (Symbol u32, thread-local Interner) |
| `sui-cache-eval` | Content-addressed eval cache (BLAKE3 keys) |
| `sui-compat` | Nix formats (NAR, store paths, ATerm, derivations) |
| `sui-store` | Store abstraction (SeaORM/SQLite) |
| `sui-build` | Build execution (sandboxed builder) |
| `sui-cache` | Binary cache (S3, local, redb) |
| `sui-daemon` | Daemon mode (worker protocol) |
| `sui-orchestrate` | System rebuild + fleet deployment |

## Laziness-First Evaluation

The evaluator's #1 principle: **never compute anything until a consumer demands it.**

CppNix forces 67 thunks for `(import <nixpkgs> {}).lib.version`. Every excess force
cascades into thousands of eval_expr calls. Maximum laziness = minimum work.

### Construction-Guaranteed Lazy Types

```rust
// Lazy<T> — impossible to access without explicit demand
let val = Lazy::defer(|| expensive_computation());
val.is_ready();  // false — no computation happened
val.demand();    // NOW it computes, caches, returns &T
val.demand();    // cached — returns immediately

// The type system prevents this:
// let x: i64 = val;  // ERROR: Lazy<i64> is not i64
// You MUST go through .demand()
```

### Evaluation Pipeline

```
Source → rnix::parse → AST
  → eval_expr (tree-walker, 16B Values, HAMT env)
    → maybe_thunk: wrap non-trivial exprs as Lazy (defer evaluation)
    → force only when consumer calls .demand() / force_value()
  OR
  → Compiler → Chunk → VM::run (8B NanBox, slot locals, TAILCALL)
    → fallback bridge: VM → tree-walker on error
```

### Critical Laziness Points

| Operation | Lazy? | Why |
|-----------|-------|-----|
| let-in binding values | YES (thunked) | Forward references need deferral |
| Attrset values | YES (thunked) | Only force when attribute accessed |
| Function arguments | YES (thunked for lambdas) | Call-by-need semantics |
| `foldl'` accumulator | Force ONE level | Strict fold — force attrset structure, NOT values |
| `//` merge | Force structure | Need keys for merge, NOT values |
| `if` condition | Force to bool | Must know which branch |
| `.` selection base | Force to attrset | Must check key exists |
| `.` selection result | **NO** — return as-is | Let caller decide when to force |

## Build & Test

```bash
cargo test --workspace          # all tests (~1500+)
cargo test -p sui-eval --lib    # eval unit tests (~1200)
cargo build --release           # optimized binary
SUI_EVAL_PERF=1 sui eval ...   # profiling (expression breakdown + thunk waste)
SUI_VM_TRACE=1 sui eval ...    # VM diagnostics (fixpoint detection, condition errors)
```

## Performance Architecture

- **Value:** 16 bytes — 2 per cache line (compile-time enforced)
- **Env:** HAMT with O(1) structural sharing — clones don't copy data
- **Thunk:** OnceCell fast-path — 150M+ cache hits skip state machine entirely
- **Strings:** SmolStr (22B inline) + Symbol(u32) interned keys
- **Allocator:** mimalloc — arena-aware, thread-caching
- **foldl':** Force accumulator after each step (matches CppNix forceValue)
- **maybe_thunk:** Literals, paths, idents, lambdas evaluated directly; everything else deferred

## Key Patterns

- **Builtin bridge:** VM delegates to tree-walker via `StringKeyedValue` conversion
- **Import fallback:** VM → tree-walker per-file on CompileError/RuntimeError
- **With-scope capture:** Thunks inside `with` blocks capture scope as upvalues
- **Fixpoint support:** `force_value` stores partial result before chain unwrap
- **Force tracking:** `force_value_tracked(val, "site_name")` for perf profiling

## Conventions

- Edition 2024, Rust 1.89.0+, MIT, `clippy::pedantic`
- Release: `codegen-units = 1`, `lto = true`, `opt-level = 3`, `strip = true`
- All code clean-room — no vendored GPL code
- `#[inline(always)]` on force_value, eval_expr fast paths
- Construction guarantees: make bad states unrepresentable via types
