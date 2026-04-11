# sui Architecture

## Crate Dependency Graph

```
sui (CLI binary)
 ├── sui-eval (tree-walker evaluator)
 │    ├── sui-intern (Symbol/Interner)
 │    ├── sui-bytecode (VM)
 │    │    └── sui-intern
 │    └── sui-compat (Nix formats)
 ├── sui-build (builder)
 │    ├── sui-eval
 │    ├── sui-store
 │    └── sui-compat
 ├── sui-store (SQLite store)
 │    └── sui-compat
 ├── sui-cache (binary cache)
 ├── sui-cache-eval (eval cache)
 ├── sui-daemon (worker protocol)
 └── sui-orchestrate (system rebuild)
```

## Dual Evaluator Architecture

sui has TWO evaluation paths that share the same `Value` type:

### Tree-Walker (sui-eval)
- Direct AST interpretation via `eval_expr_inner`
- Tail-call trampoline loop (IfElse, LetIn, With, Assert → continue)
- `Value` enum: 16 bytes, Rc-wrapped heap variants
- Environment: `im_rc::HashMap<Symbol, Value, FxBuildHasher>` (persistent HAMT)
- Thunks: `OnceCell` fast-path + `UnsafeCell` state machine
- Performance: exceeds CppNix 3x on 45/48 benchmarks

### Bytecode VM (sui-bytecode)
- Two-phase: Compiler → Chunk → VM execution
- `NanBox`: 8-byte IEEE 754 NaN-payload encoding
- 42+ opcodes including TAILCALL (frame reuse)
- Slot-based locals: O(1) array index (vs HAMT O(log32 n))
- Import fallback: catches CompileError/RuntimeError → tree-walker
- Builtin bridge: `StringKeyedValue::Callable` wraps tree-walker builtins

### Fallback Bridge
The VM tries first. On failure, falls back to tree-walker per-file:
```
VM compile file → success → VM execute
                → CompileError → tree-walker evaluates file
                → RuntimeError → tree-walker evaluates file
```

## Value Representation

```
Tree-walker Value (16 bytes):
  Null | Bool(bool) | Int(i64) | Float(f64)     — inline
  String(Rc<NixString>) | Path(Box<SmolStr>)     — pointer
  List(Rc<Vec<Value>>) | Attrs(Rc<NixAttrs>)     — pointer
  Lambda(Box<Closure>) | Builtin(Box<BuiltinFn>) — pointer
  Thunk(Rc<ThunkInner>)                           — pointer

VM NanBox (8 bytes):
  Float64 stored as-is (IEEE 754)
  Null=0x0, False=0x1, True=0x2, Int48=0x3       — tagged
  Pointer=0x4 → HeapObject enum                   — heap
```

## Key Performance Patterns

- **maybe_thunk**: Skip thunking literals, paths, lambdas (CppNix maybeThunk)
- **OnceCell thunk cache**: 150M+ cache hits bypass UnsafeCell entirely
- **FxHash**: rustc-hash for Symbol(u32) keys (~4x faster than SipHash)
- **Lazy mapAttrs**: Deferred thunk wrapping (matches CppNix semantics)
- **Deep equality**: Recursively forces thunks in == and builtins.elem
- **Stacker skip**: Ident/Literal/Paren bypass stacker::maybe_grow

## String Interning (sui-intern)

All attribute names and variable names are interned:
```
"hello" → intern("hello") → Symbol(42)
Symbol comparison: single u32 == (O(1))
Thread-local Interner: HashMap<String, Symbol> + Vec<String>
```

## Evaluation Cache (sui-cache-eval)

Content-addressed memoization:
```
BLAKE3(source) + BLAKE3(flake.lock) → CacheKey
CacheKey → CachedValue (JSON + timestamp)
In-memory HashMap + optional JSON persistence
```
