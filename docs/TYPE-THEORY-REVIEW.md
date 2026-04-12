# Type-Theoretic Review of sui Evaluator

## Purpose

Map every concept in the evaluator to a type. Identify every state that should
be IMPOSSIBLE but is currently representable. Define types that eliminate them.

## The 8 Invalid States

### 1. `Concrete(Value::Thunk(_))` — CRITICAL

**Where:** `Value::demand()` wraps `force_value()` result without type-level check.
**Defense:** `debug_assert` only — silent in release builds.
**Fix:** Make `Concrete` a separate enum without a Thunk variant.

```rust
// CURRENT (broken in release):
pub struct Concrete(Value);  // can secretly hold Thunk

// FIXED:
pub enum Concrete {
    Null, Bool(bool), Int(i64), Float(f64),
    String(Rc<NixString>), Path(Box<SmolStr>),
    List(Rc<Vec<Value>>), Attrs(Rc<NixAttrs>),
    Lambda(Box<Closure>), Builtin(Box<BuiltinFn>),
}
// Thunk variant DOES NOT EXIST. Compiler rejects construction.
```

### 2. `ThunkRepr::Evaluated(Value::Thunk(_))` — BUG

**Where:** Thunk force inner stores result, then unwraps transitively. Depth limit
can leave `Evaluated(Null)` instead of the real value.
**Fix:** Store `Concrete` inside `Evaluated`, not `Value`.

```rust
// CURRENT:
Evaluated(Box<Value>)  // can hold another Thunk

// FIXED:
Evaluated(Box<Concrete>)  // compiler prevents Thunk storage
```

### 3. List elements are `Value` (may contain Thunks)

**Where:** `Value::List(Rc<Vec<Value>>)` — elements can be thunks.
**Issue:** Nix lists ARE lazy (elements evaluated on access). So thunks in
lists are CORRECT semantics. But operations like `length` shouldn't force elements.
**Fix:** Keep `Value` elements but ensure list operations don't force them
unless accessing specific elements. Document the contract.

### 4. `force_value()` returns `Value` (not `Concrete`)

**Where:** Every call site of `force_value` receives a `Value` that is
SUPPOSED to be concrete but isn't type-checked.
**Fix:** Change return type to `Concrete`.

```rust
// CURRENT:
pub fn force_value(value: &Value) -> Result<Value, EvalError>

// FIXED:
pub fn force_value(value: &Value) -> Result<Concrete, EvalError>
```

### 5. `apply()` returns `Value` (may be Thunk)

**Where:** `apply(func, arg)` returns `eval_expr(&closure.body, &call_env)` which
can be a Thunk.
**Status:** This is CORRECT — apply returns a lazy result. Callers that need
concrete must demand. No fix needed.

### 6. `NixAttrs::get()` returns `&Value` (may be Thunk)

**Where:** Attribute access returns potentially-lazy values.
**Status:** CORRECT for Nix semantics — attrset values ARE lazy.
**Fix:** The API is correct. Callers must `.demand()` when they need concrete.

### 7. Blackhole left on panic

**Where:** If evaluator panics during thunk force, the thunk stays `Blackhole`.
**Status:** Theoretical — Rust panics are caught by `catch_unwind` in tests.
**Fix:** Not feasible at the type level. Use `scopeguard` for cleanup.

### 8. Binding references undefined variable

**Where:** `maybe_thunk` wraps expressions whose free variables may not be in scope.
**Status:** CORRECT — Nix defers undefined-variable errors until force.
**Fix:** None needed. Runtime error on force is the Nix spec.

## The Type Hierarchy

```
Value                    ← what the evaluator produces (may be lazy)
  ├─ Concrete variants   ← null, bool, int, float, string, path, list, attrs, lambda, builtin
  └─ Thunk               ← deferred computation

Concrete                 ← what consumers receive after .demand() (NEVER lazy)
  ├─ all concrete variants
  └─ NO Thunk variant    ← compiler enforces

NixAttrs                 ← key-value map where values are Value (lazy)
  ├─ keys() → no forcing
  ├─ get() → &Value (caller demands)
  └─ update() → no forcing

Env                      ← scope chain
  ├─ bindings → Value (lazy)
  ├─ with_scopes → lazy (forced on first lookup)
  └─ child() → O(1) clone
```

## The 3 Changes That Eliminate All Invalid States

### Change 1: `Concrete` becomes an enum (not a wrapper)

```rust
pub enum Concrete {
    Null, Bool(bool), Int(i64), Float(f64),
    String(Rc<NixString>), Path(Box<SmolStr>),
    List(Rc<Vec<Value>>), Attrs(Rc<NixAttrs>),
    Lambda(Box<Closure>), Builtin(Box<BuiltinFn>),
}
```

`Value::demand()` pattern-matches and constructs `Concrete` variant-by-variant.
Compiler rejects `Concrete::Thunk(...)` because the variant doesn't exist.

### Change 2: `force_value()` returns `Concrete`

```rust
pub fn force_value(value: &Value) -> Result<Concrete, EvalError>
```

Every call site that currently does:
```rust
let forced = force_value(&val)?;
let attrs = forced.as_attrs()?;
```
Becomes:
```rust
let concrete = force_value(&val)?;
let attrs = concrete.as_attrs()?;  // guaranteed no thunk
```

### Change 3: `ThunkRepr::Evaluated` stores `Concrete`

```rust
pub enum ThunkRepr {
    Suspended { expr: Expr, env: Env },
    InheritSelect { source: Thunk, name: SmolStr },
    Native(Box<dyn FnOnce() -> Result<Concrete, EvalError>>),
    Blackhole,
    Evaluated(Box<Concrete>),  // ← guaranteed non-thunk
}
```

## What This Eliminates

| Invalid State | Eliminated By | How |
|---|---|---|
| `Concrete(Thunk)` | Change 1 | Variant doesn't exist |
| `Evaluated(Thunk)` | Change 3 | Inner type is `Concrete` |
| `force_value → Thunk` | Change 2 | Return type is `Concrete` |
| `demand → Thunk` | Change 1 | Constructs `Concrete` enum |
| Accidental forcing | Changes 1+2 | Compiler requires `.demand()` |

## What This Does NOT Change

- `Value::List(Rc<Vec<Value>>)` — list elements stay lazy (correct)
- `NixAttrs(FxHashMap<Symbol, Value>)` — attrset values stay lazy (correct)
- `apply()` returns `Value` — function results stay lazy (correct)
- `eval_expr()` returns `Value` — evaluation results stay lazy (correct)
- The VM (`sui-bytecode`) — separate value system, unchanged
