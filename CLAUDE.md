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

## ★★ North star — the nix→sui flip (destination-first, no hacks in the evaluator)

sui replaces nix at the **binary level**: the fleet's default rebuild
(`nix run .#rebuild`, darwin-/nixos-rebuild) will invoke sui over the **same
shared `/nix/store`** — byte-identical hash/NAR/drv/binary-cache formats
(proven at `sui parity` 7/0). The flip is the destination, not a maybe; a
change moves toward it or is off-course.

**The bar is 100%, byte for byte — table stakes, not a goal.** A replacement that
matches 99% of the ecosystem is not a nix replacement; the missing 1% is exactly
where a real system silently mis-builds. Standing invariant, now and across every
future version of sui as a binary-delivery platform: for the ENTIRE ecosystem sui
touches — nixpkgs, darwin-nix, nixos, home-manager — sui's `outPath` / `.drv` /
NAR / realized-store bytes are **identical to nix's, or it is a defect**, never an
acceptable state. **Silent divergence** — a wrong value that still evaluates — is
the worst failure of all, worse than a crash, because it corrupts a build with no
signal; that is why a band-aid that "makes eval proceed" is forbidden (it trades a
loud gap for a silent one).

**Proven, not asserted — the enforced form of this fact is `sui parity` extended
to a corpus gate** (the typed vocabulary + phased plan: [`docs/BYTE-PARITY-TYPESCAPE.md`](docs/BYTE-PARITY-TYPESCAPE.md))**:** a differential that evaluates a growing corpus (nixpkgs
packages → whole nixpkgs → system closures) in sui *and* nix and byte-compares
every artifact, going red on a single differing byte. `sui == nix` is then a
theorem the gate re-proves on every change — the mechanical observation of this
fact over sui's evolution. Grow the gate's coverage with the evaluator so a green
build never outruns the corpus it actually proves.

Getting there is empirical — stock nix is the differential **oracle** (see
*The sui rhythm* below). Every fix, **especially in the subtle
evaluator/fixpoint code**, is elegant Rust + tatara-lisp, macro vocabulary
where a pattern repeats, no tech debt. A band-aid in the evaluator — returning
`null` "so eval proceeds", memoizing a transient fixpoint partial, an
env-var-gated sentinel — is **not a fix**: it hides a divergence the oracle
will resurface downstream. Fix the load-bearing cause (e.g. the genuine
`Promise(NixAttrs)` handling of a blackholed *lexical* binding in the
module-system fixpoint), never paper over it. This is Operating Principle #0
(path-of-least-resistance is a cardinal sin) at the evaluator.

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

## The sui rhythm — the empirical fix loop

The cadence for any sui/nix divergence: **repro → oracle → instrument → pin →
root-fix → verify+test → commit → next.** Each pass drains one bug class and
leaves a passing regression test. Never guess-patch the evaluator.

1. **Repro small.** Reduce to the cheapest expression that still fails. For the
   module fix-point, the leaf
   `(builtins.getFlake "path:.../nix").darwinConfigurations.<host>.config.system.stateVersion`
   forces the full merge without package realization (minutes, not the whole system).
2. **Oracle.** Stock nix is ground truth — `nix eval … --raw` and
   `builtins.typeOf <name>` nix-vs-sui. Kill wrong hypotheses with minimal-expression
   probes (`sui eval --no-vm -E '<expr>'` — `--no-vm` is the tree-walker; drop it for
   the VM path) before touching the evaluator.
3. **Instrument at the site, env-gated** — `if std::env::var_os("SUI_DBG_X").is_some() { eprintln!(…) }`;
   `cargo build --release --bin sui` (~3.5 min); run; read. Techniques that cracked
   real bugs: instrument the **builtin itself** with `crate::eval::current_eval_file()`
   — a builtin error's file is often where it was *bound* (`inherit (lib) …`), not
   called; check **every** failure path (`concatLists` has two `as_list()` — the null
   was an *element*, not the arg); dump the exact expr across files via
   `ident.syntax().ancestors().nth(2).text()`, or a scope's names via a `debug_keys()`
   env probe.
4. **Force-order test.** A value that's `null` forced lazily but a list forced
   eagerly (`map builtins.typeOf` trace) is a fix-point order-dependence — the M2.6
   Promise/Blackhole partial-value class. Deep; don't hack it.
5. **Missing-global diff.** For `UndefinedVar`, diff nix bare-globals against sui's
   `builtins/mod.rs` `DEFAULT_SCOPE`; promote genuine nix globals that exist under
   `builtins.` but aren't bare (`placeholder`, `fetchGit`, `fromTOML`, …).
6. **Root-fix, no hacks** (the north-star): a band-aid — return null so eval
   proceeds, memoize a transient partial, an env-sentinel — is not a fix.
7. **Verify + regression-test.** Revert every diagnostic; keep only the fix; run
   the `sui-eval` suite. Prove any remaining failures are pre-existing by checking
   out the pre-session commit and re-running (env-dependent flake/NIX_PATH tests
   fail regardless). Add one passing test per fix (unit-test a private fn via
   `rnix::Root::parse(s).tree().expr()`). Commit.

**Cache-edit discipline.** To site-trace inside nixpkgs: `cp` the cached lib file
to a backup, `chmod u+w`, add `builtins.trace` wrappers (no rebuild — nix-file
change), run, then force-restore. Always leave `~/.cache/sui` pristine.

**Crash → lldb.** Stack overflow / segfault: ad-hoc-codesign the binary
(get-task-allow), run under `lldb -k` (run-on-crash), on a
`CARGO_PROFILE_RELEASE_STRIP=false DEBUG=1` build for symbols.

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
