# Construction Guarantees — Build-Time Provable Software

## Philosophy

Every software quality that matters can be encoded as a TYPE.
If the type is correct, the quality is guaranteed. If the type is wrong, the compiler rejects it.
No runtime checks. No defensive programming. No "hope it works."

**The principle:** Make bad states unrepresentable.

## Catalog of Build-Time Guarantees

### Performance Guarantees

| Quality | Type / Mechanism | What it prevents | Example |
|---------|-----------------|------------------|---------|
| **Laziness** | `Lazy<T>` — `.demand()` required | Accidental eager evaluation | `Lazy::defer(\|\| expensive())` |
| **Memoization** | `OnceCell<T>` — set exactly once | Redundant computation | Thunk cache: 150M hits |
| **Cache locality** | `assert!(size_of::<T>() <= N)` | Values too large for cache lines | `Value ≤ 16B` |
| **Zero-copy sharing** | `Rc<T>` / `Arc<T>` / HAMT | Deep copies in hot paths | Env clone is O(1) Rc bump |
| **Interning** | `Symbol(u32)` newtype | String alloc in comparisons | Attrset key lookup: u32 == u32 |
| **Arena allocation** | `Arena<'a, T>` lifetime-scoped | Fragmentation + GC pauses | Per-eval-scope bulk dealloc |
| **Bounded recursion** | `stacker::maybe_grow` | Stack overflow on deep ASTs | nixpkgs 50+ overlay depth |

### Correctness Guarantees

| Quality | Type / Mechanism | What it prevents | Example |
|---------|-----------------|------------------|---------|
| **Null safety** | `Option<T>` — no null pointers | NullPointerException | Nix's `null` is a Value variant |
| **Error propagation** | `Result<T, E>` — must handle errors | Unchecked exceptions | Every eval returns `Result<Value, EvalError>` |
| **Exhaustive matching** | `match` on enums — compiler checks all variants | Missing cases | Value enum: all 11 variants handled |
| **Ownership** | Move semantics — single owner | Use-after-free, double-free | Value ownership transfers on bind |
| **Borrowing** | `&T` / `&mut T` — compile-time borrow check | Data races, aliased mutation | Env is `Rc<EnvInner>` with CoW |
| **Thread safety** | `!Send` / `!Sync` on `Rc` | Cross-thread data races | Evaluator is explicitly single-threaded |
| **Infinite recursion** | Blackhole detection in ThunkRepr | `let x = x; in x` hangs | Blackhole → InfiniteRecursion error |
| **Fixpoint soundness** | Store-partial-first in force_value | False blackhole on fixpoints | nixpkgs `lib.fix` works |

### Security Guarantees

| Quality | Type / Mechanism | What it prevents | Example |
|---------|-----------------|------------------|---------|
| **Sandbox isolation** | Builder trait — no ambient authority | Impure builds | Network/filesystem restricted |
| **Content addressing** | BLAKE3 hash as CacheKey | Cache poisoning | Same input → same output |
| **Secret isolation** | SOPS-encrypted secrets — typed paths | Secret leakage | `sops.secrets.NAME.path` |
| **Hermetic evaluation** | Pure mode — no impure builtins | Non-reproducible results | `--pure-eval` flag |

### Concurrency Guarantees

| Quality | Type / Mechanism | What it prevents | Example |
|---------|-----------------|------------------|---------|
| **Lock-free reads** | `OnceCell` — no mutex on read path | Lock contention | Thunk cache reads: zero locks |
| **Ordered messaging** | NATS JetStream delivery | Message loss, reordering | Build agent protocol |
| **Atomic transitions** | `Cell::take()` + `Cell::set()` | Torn state reads | Thunk state machine |

### Determinism Guarantees

| Quality | Type / Mechanism | What it prevents | Example |
|---------|-----------------|------------------|---------|
| **Reproducible builds** | Content-addressed store paths | "Works on my machine" | Same derivation → same output |
| **Reproducible eval** | BLAKE3-keyed eval cache | Eval non-determinism | Same source → same result |
| **Sorted output** | `BTreeMap` / sorted iteration | Non-deterministic key order | Attrset output is always sorted |

## The Pattern

Every guarantee follows the same recipe:

```
1. Identify the invariant (e.g., "values are only computed when needed")
2. Encode it as a TYPE (e.g., Lazy<T>)
3. Make the good path easy (.demand() is obvious)
4. Make the bad path impossible (can't access T without .demand())
5. Let the compiler enforce it (type error on misuse)
6. Test the guarantee (property tests, not just unit tests)
7. Document WHY (not just what)
```

## Applying to New Domains

The same pattern conquers every domain:

| Domain | Invariant | Type |
|--------|-----------|------|
| **Evaluation** | Values computed on demand | `Lazy<Value>` |
| **Caching** | Same input → same output | `ContentAddressed<Hash, T>` |
| **Building** | No impure inputs | `Hermetic<Build>` |
| **Networking** | Authenticated requests only | `Authenticated<Request>` |
| **Storage** | Signed artifacts only | `Signed<Artifact, Key>` |
| **Concurrency** | No data races | `Shared<T>` (Arc + interior mutability) |
| **Configuration** | Valid config only | `Validated<Config>` (parse, don't validate) |
| **Deployment** | Attested artifacts only | `Attested<Deployment>` (tameshi) |

Each type is a zero-cost abstraction — the wrapper compiles away.
What remains is the guarantee, enforced at every call site, forever.

## Proving Guarantees with Tests

Each construction guarantee has a corresponding test strategy:

```rust
// Property: Lazy values are never computed until demanded
#[test]
fn lazy_not_computed_until_demand() {
    let computed = Cell::new(false);
    let lazy = Lazy::defer(|| { computed.set(true); 42 });
    assert!(!computed.get());  // NOT computed
    lazy.demand();
    assert!(computed.get());   // NOW computed
}

// Property: Thunks evaluate exactly once
#[test]
fn thunk_evaluates_once() {
    let count = Cell::new(0);
    let thunk = Thunk::new_suspended(expr, env);
    thunk.force(evaluator);
    thunk.force(evaluator);  // cache hit
    assert_eq!(count.get(), 1);  // evaluated exactly once
}

// Property: Value fits in 16 bytes (cache-friendly)
const _: () = assert!(std::mem::size_of::<Value>() <= 16);
// This is checked at COMPILE TIME. No test needed.
```
