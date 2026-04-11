# Sui (粋) — Rust-Native Nix Replacement

Pure-Rust Nix evaluator + build system. Drop-in `nix` CLI replacement (`alias nix=sui`).
Tree-walker exceeds CppNix 3x on 45/48 benchmarks. Bytecode VM with NaN-boxed 8-byte values.

## Workspace (11 crates)

| Crate | Purpose |
|-------|---------|
| `sui` (root) | CLI binary — nix-compatible interface |
| `sui-eval` | Tree-walker evaluator (Value 16B, Env, Thunk, NixAttrs) |
| `sui-bytecode` | Bytecode VM (NanBox 8B, 44+ opcodes, TAILCALL, slot locals) |
| `sui-intern` | Shared string interning (Symbol u32, Interner, thread-local) |
| `sui-cache-eval` | Content-addressed eval cache (BLAKE3, JSON persistence) |
| `sui-compat` | Nix formats (NAR, store paths, derivations, ATerm) |
| `sui-store` | Store abstraction (SeaORM/SQLite) |
| `sui-build` | Build execution (sandboxed builder) |
| `sui-cache` | Binary cache (S3, local, redb) |
| `sui-daemon` | Daemon mode (worker protocol) |
| `sui-orchestrate` | System rebuild + deployment |

## Evaluation Pipeline

```
Source → rnix::parse → AST
  → eval_expr_inner (tree-walker, 16B Values, HAMT env)
  OR
  → Compiler → Chunk → VM::run (8B NanBox, slot locals, TAILCALL)
  ↕ (fallback bridge: VM → tree-walker on CompileError/RuntimeError)
  → force_value → Result<Value>
```

## Build & Test

```bash
cargo test --workspace          # all tests (~3000+)
cargo test -p sui-eval --lib    # eval unit tests (~1200)
cargo test -p sui-eval --test perf_regression  # 19 perf regression tests
cargo build --release           # optimized binary
SUI_EVAL_PERF=1 sui eval ...   # profiling mode (expression breakdown + thunk waste)
```

## Performance Architecture

- **Value:** 16 bytes (Rc-wrapped String/Attrs/Lambda/Builtin, Box-wrapped Path)
- **Env:** im_rc::HashMap<Symbol, Value, FxBuildHasher> — O(log32 n) persistent HAMT
- **Thunk:** OnceCell fast-path (150M+ cache hits bypass UnsafeCell) + state machine
- **Strings:** SmolStr (22B inline), interned Symbol(u32) keys
- **Lists:** Rc<Vec<Value>> — O(1) clone
- **Allocator:** mimalloc global allocator
- **VM NanBox:** 8-byte IEEE 754 NaN-payload encoding (null/bool/int48/float/heap pointer)
- **maybe_thunk:** Skip thunking literals, paths, lambdas (CppNix maybeThunk equivalent)

## Key Patterns

- **Error helpers:** `EvalError::builtin_type(name, expected, got)`, `EvalError::op_type(op, lhs, rhs)`
- **Builtin bridge:** VM calls tree-walker builtins via `StringKeyedValue::Callable` + `set_builtin_bridge()`
- **Import fallback:** VM catches CompileError AND RuntimeError, falls back to tree-walker per-file
- **Perf counters:** `crate::perf::inc(Counter::EvalExpr)` — enum-indexed array, zero-cost when disabled

## Conventions

- Edition 2024, Rust 1.89.0+, MIT, `clippy::pedantic`
- Release: `codegen-units = 1`, `lto = true`, `opt-level = 3`, `strip = true`
- All code clean-room — no vendored GPL code
- `#[inline(always)]` on force_value, eval_expr fast paths
- 19 unsafe blocks in value.rs — all justified with SAFETY comments
- Compile-time assertion: `size_of::<Value>() <= 16`
