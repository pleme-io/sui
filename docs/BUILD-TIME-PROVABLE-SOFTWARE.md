# Build-Time Provable Software

## The Theory

Software has qualities: performance, correctness, security, determinism, laziness.
Traditionally these are verified at runtime — tests, benchmarks, monitoring.
Runtime verification is necessary but insufficient: it only proves the tested paths.

**Build-time provable software** encodes qualities as TYPES. The compiler verifies
ALL paths. If the code compiles, the quality is guaranteed. Not "tested." PROVED.

This is not theoretical. Rust already does it for memory safety (ownership + borrow checker).
We generalize the pattern to EVERY quality that matters.

## The Pattern

```
Quality → Type → Compiler Enforcement → Guarantee
```

1. **Identify** the quality you need (e.g., "values are only computed when needed")
2. **Encode** it as a wrapper type (e.g., `Lazy<T>`)
3. **Restrict** the API so the bad path is unrepresentable (no way to access T without `.demand()`)
4. **Compose** — types compose. `Lazy<Shared<Interned<Value>>>` gives you laziness + sharing + interning.
5. **Distribute** — publish the type as a library. Every consumer inherits the guarantee.

## The Library Approach

Each quality becomes a reusable library. The library exports ONE type with ONE guarantee.
Consumers depend on the library. The guarantee propagates through the dependency graph.

```
┌────────────────────────────────────────────────────┐
│ Application (sui, mado, hiroba, etc.)              │
│   Uses: Lazy<Value>, Interned<String>, Arena<Env>  │
├────────────────────────────────────────────────────┤
│ Guarantee Libraries                                 │
│   sui-lazy    → Lazy<T>         (laziness)         │
│   sui-intern  → Symbol(u32)     (interning)        │
│   hayai       → RegexMatcher    (fast matching)    │
│   shikumi     → ConfigStore<T>  (config validity)  │
│   kenshou     → AuthProvider    (authentication)   │
│   tameshi     → MerkleTree      (integrity)        │
├────────────────────────────────────────────────────┤
│ Rust Compiler                                       │
│   Enforces: ownership, borrowing, lifetimes, types │
│   Rejects: use-after-free, data races, type errors │
└────────────────────────────────────────────────────┘
```

## Catalog of Provable Qualities

### Tier 1: Rust Gives Us For Free
These are enforced by the Rust compiler without any library code:

| Quality | Mechanism | Guarantee |
|---------|-----------|-----------|
| Memory safety | Ownership + borrow checker | No use-after-free, no double-free |
| Thread safety | Send + Sync traits | No data races |
| Null safety | Option<T> | No null pointer dereference |
| Error handling | Result<T, E> | No unchecked exceptions |
| Exhaustiveness | match + #[non_exhaustive] | No missing cases |
| Type safety | Strong static typing | No type confusion |

### Tier 2: We Build with Wrapper Types
These require a small library (one type + restricted API):

| Quality | Type | API | Prevents |
|---------|------|-----|----------|
| Laziness | `Lazy<T>` | `.demand()` to access | Accidental eager evaluation |
| Memoization | `Once<T>` | `.get_or_init()` | Redundant computation |
| Interning | `Interned<str>` | `Interned::new()` | Duplicate string allocation |
| Cache locality | `Compact<T, N>` | compile-time `size_of` assert | Cache-hostile value bloat |
| Structural sharing | `Shared<T>` | `.update()` returns new version | Deep copies |
| Arena allocation | `Arena<'a, T>` | `arena.alloc()` | Fragmentation, GC pauses |
| Bounded recursion | `Depth<T, N>` | compile-time depth limit | Stack overflow |
| Monotonic time | `Instant` (std) | Cannot go backwards | Time confusion |

### Tier 3: We Build with Trait Constraints
These require a trait that types must implement:

| Quality | Trait | Requirement | Prevents |
|---------|-------|-------------|----------|
| Determinism | `Deterministic` | Same input → same output | Non-reproducible builds |
| Content-addressing | `ContentAddressed` | Hash uniquely identifies content | Cache poisoning |
| Serializability | `Serialize + Deserialize` | Can round-trip through storage | Data loss |
| Hermetic evaluation | `Pure` | No ambient authority | Impure side effects |
| Idempotency | `Idempotent` | Applying twice = applying once | Double-application bugs |
| Commutativity | `Commutative` | Order doesn't matter | Order-dependent bugs |

### Tier 4: We Build with Protocol Types (State Machines)
These require a type-state pattern where invalid transitions are unrepresentable:

| Quality | State Machine | States | Prevents |
|---------|--------------|--------|----------|
| Build lifecycle | `Build<Configured> → Build<Built> → Build<Tested>` | Source → Compiled → Verified | Deploying untested code |
| Auth flow | `Unauthenticated → Authenticated<Token>` | Anonymous → Verified | Unauthorized access |
| Deployment | `Attested<Unsigned> → Attested<Signed>` | Built → Integrity-verified | Deploying unsigned artifacts |
| Thunk lifecycle | `Suspended → Evaluating → Evaluated` | Deferred → In-progress → Done | Double evaluation, blackhole |
| Connection | `Connecting → Connected → Authenticated` | TCP → TLS → Authed | Using unauthed connections |

## How This Applies to Every pleme-io Library

Every library in the pleme-io ecosystem follows this pattern:

### Library Scope Contract
Each library declares:
- **What it DOES** (its construction guarantee)
- **What it does NOT do** (what consumers must provide)
- **What types enforce the boundary** (the API surface)

Example — `shikumi` (configuration):
```
DOES: Config file discovery, loading, hot-reload, typed access via ConfigStore<T>
DOES NOT: Application logic, validation rules, secret management
TYPES: ConfigStore<T: DeserializeOwned> — guarantees T is valid config structure
```

Example — `kenshou` (authentication):
```
DOES: OAuth2/OIDC flows, token validation, session management
DOES NOT: Authorization (what users can do), business logic
TYPES: AuthProvider trait — guarantees authentication happened before access
```

Example — `hayai` (fast matching):
```
DOES: Normalize → Prefilter → RegexSet DFA matching, with caching
DOES NOT: Domain-specific rules, scoring, ranking
TYPES: RegexMatcher<N: Normalizer, P: Prefilter> — guarantees normalization before match
```

### The Redistributability Principle

When a guarantee is encoded as a type in a library:
1. Every consumer of the library inherits the guarantee
2. The guarantee composes with other guarantees
3. The guarantee is verified at every consumer's compile time
4. The guarantee cannot be accidentally bypassed

This means: **build one correct library, distribute the guarantee to every application.**

## Applying This to Conquer Performance

The specific application to sui's evaluation performance:

```rust
// BEFORE: Value might be a thunk. Callers must check.
enum Value { ..., Thunk(Thunk) }
// Bug: caller forgets to force → wrong behavior
// Bug: caller forces eagerly → performance regression

// AFTER: Lazy<Value> enforces the contract.
struct LazyValue(Lazy<ConcreteValue>);
// .demand() to get concrete value — compiler requires it
// Cannot accidentally skip forcing (type error)
// Cannot accidentally force eagerly (must call .demand())
```

The same pattern conquers every performance domain:
- **Caching:** `Cached<K, V>` — lookup before compute (compiler-enforced)
- **Batching:** `Batch<T, N>` — accumulate before flush (type prevents unbatched ops)
- **Pooling:** `Pooled<T>` — reuse before allocate (type prevents raw allocation)

## The Vision

Every pleme-io library is a construction guarantee.
Every application composes guarantees.
The compiler proves the composition is valid.
Bad states are unrepresentable at every level.

This is how you build software that is **correct by construction** — not correct by testing, not correct by review, but correct because the TYPE SYSTEM makes incorrectness impossible to express.
