# sui Architecture

## Design Principle: Construction Guarantees

Every performance-critical invariant is encoded in Rust's type system.
Bad states are unrepresentable. The compiler enforces what documentation promises.

```
Lazy<T>     — value only computed on .demand()     → prevents accidental eagerness
Symbol(u32) — interned string identity              → prevents allocation in hot loops
Rc<NixAttrs> — structural sharing                   → prevents O(n) attrset copies
OnceCell    — exactly-once evaluation               → prevents redundant computation
Value ≤ 16B — compile-time size assertion            → prevents cache line waste
```

## Crate Dependency Graph

```
sui (CLI binary)
 ├── sui-eval (tree-walker + lazy primitives)
 │    ├── sui-intern (Symbol/Interner)
 │    ├── sui-bytecode (VM)
 │    │    └── sui-intern
 │    └── sui-compat (Nix formats)
 ├── sui-build (sandboxed builder)
 │    ├── sui-eval
 │    ├── sui-store
 │    └── sui-compat
 ├── sui-store (SQLite store)
 │    └── sui-compat
 ├── sui-cache (binary cache)
 ├── sui-cache-eval (BLAKE3 eval cache)
 ├── sui-daemon (worker protocol)
 └── sui-orchestrate (system rebuild)
```

## Laziness-First Evaluation

The evaluator's core architecture is built around MAXIMUM LAZINESS.
CppNix forces 67 thunks for `lib.version`. Every excess force cascades.

### The Lazy<T> Foundation (sui-eval/src/lazy.rs)

```rust
pub struct Lazy<T: Clone> {
    inner: Rc<LazyInner<T>>,
}
// - Lazy::defer(|| computation)  — deferred until .demand()
// - Lazy::ready(value)           — already computed
// - .demand() -> &T              — explicit force (compile-time opt-in)
// - .is_ready() -> bool          — check without forcing
// - Clone shares computation     — Rc-backed, single eval
```

The type system prevents accidental forcing. You CANNOT pattern-match
past `Lazy<T>` to get `T` — you must call `.demand()`.

### Dual Evaluator Architecture

#### Tree-Walker (sui-eval)
- Direct AST interpretation via `eval_expr_inner`
- Tail-call trampoline (IfElse, LetIn, With, Assert → continue)
- `Value` enum: 16 bytes, Rc-wrapped heap variants
- Environment: HAMT with O(1) structural clone
- Thunks: OnceCell fast-path + UnsafeCell state machine
- maybe_thunk: wraps non-trivial expressions as deferred thunks

#### Bytecode VM (sui-bytecode)
- Compiler → Chunk → VM execution
- NanBox: 8-byte IEEE 754 NaN-payload encoding
- 44+ opcodes including TAILCALL
- With-scope capture: thunks inside `with` blocks capture scope as upvalues
- Fixpoint: store partial result as Done before chain unwrap

#### Fallback Bridge
```
VM compile → success → VM execute
           → CompileError → tree-walker per-file
           → RuntimeError → tree-walker per-file
```

## Value Representation

```
Tree-walker Value (16 bytes, compile-time enforced):
  Null | Bool(bool) | Int(i64) | Float(f64)     — inline (zero alloc)
  String(Rc<NixString>) | Path(Box<SmolStr>)     — pointer
  List(Rc<Vec<Value>>) | Attrs(Rc<NixAttrs>)     — pointer (structural sharing)
  Lambda(Box<Closure>) | Builtin(Box<BuiltinFn>) — pointer
  Thunk(Rc<ThunkInner>)                           — lazy (OnceCell + state machine)

VM NanBox (8 bytes):
  Float64 as-is | Null=0x0 | False=0x1 | True=0x2 | Int48=0x3 | Heap=0x4
```

## Performance-Critical Construction Guarantees

### 1. Thunk Memoization (OnceCell)
```rust
// First access: evaluate and store
let result = thunk.force(evaluator)?;  // Suspended → Blackhole → Evaluated
// All subsequent accesses:
if let Some(cached) = self.0.cache.get() {  // OnceCell fast path
    return Ok((**cached).clone());  // No UnsafeCell access
}
// 150M+ cache hits bypass the state machine entirely
```

### 2. Environment Sharing (HAMT)
```rust
// child() is O(1) — just Rc bump, no data copy
pub fn child(&self) -> Self {
    Self(Rc::new(EnvInner {
        bindings: self.0.bindings.clone(), // HAMT structural sharing
        ...
    }))
}
// Lookup is O(log32 n) — single HAMT walk, no chain traversal
```

### 3. String Interning (Symbol)
```rust
// All attrset keys are interned at creation:
"hello" → intern("hello") → Symbol(42)
// Comparison: Symbol(42) == Symbol(42)  — single u32 compare
// No string allocation, no hash computation in hot loops
```

### 4. Strict-Where-Required, Lazy-Everywhere-Else
```rust
// foldl' forces accumulator (matches CppNix forceValue):
acc = force_value(&apply(partial, v.clone())?)?;
// BUT: attrset VALUES in the accumulator stay as thunks
// Only individual attribute access forces individual values
```

## Content-Addressed Caching (sui-cache-eval)

```
BLAKE3(source_text) + BLAKE3(flake.lock) → CacheKey
CacheKey → CachedValue (JSON + timestamp)
Same input → same output → same key → reusable across evaluations
```

## Construction Guarantee Roadmap

| Type | Invariant | Status |
|------|-----------|--------|
| `Lazy<T>` | No accidental forcing | Implemented |
| `Value ≤ 16B` | Cache-friendly values | Enforced |
| `Symbol(u32)` | O(1) string comparison | Enforced |
| `OnceCell` | Exactly-once thunk eval | Enforced |
| `Arena<T>` | Bulk alloc/dealloc | Planned |
| `Interned<str>` | Zero-dup strings | Planned |
| `Hermetic<Build>` | No impure inputs | Planned |
