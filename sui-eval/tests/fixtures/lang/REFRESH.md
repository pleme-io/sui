# Language test corpus

This directory holds hand-curated Nix language fixtures used by
`sui-eval/tests/lang_corpus.rs`. Each test consists of two files:

- `eval-okay-<name>.nix` — the Nix expression to evaluate
- `eval-okay-<name>.exp` — the expected `serde_json::Value` output,
  one JSON value, no trailing newline restriction

The runner reads each `.nix`, evaluates it via `sui_eval::eval`,
converts the result with `Value::to_json`, and compares against the
parsed `.exp`. In online mode (`SUI_TEST_ONLINE=1`) the runner
*additionally* invokes `nix-instantiate --eval --json --strict <file>`
and asserts the oracle agrees with both sides.

## Refreshing from the CppNix upstream corpus

CppNix ships a much larger canonical corpus at
`nix/tests/functional/lang/` (formerly `nix/tests/lang/`). Vendoring
it here is a single-shot import, not a build-time fetch. To refresh:

```bash
# From anywhere with network access:
nix shell nixpkgs#git --command \
  git clone --depth 1 https://github.com/NixOS/nix /tmp/nix
cp /tmp/nix/tests/functional/lang/eval-okay-*.nix \
   ~/code/github/pleme-io/sui/sui-eval/tests/fixtures/lang/
cp /tmp/nix/tests/functional/lang/eval-okay-*.exp \
   ~/code/github/pleme-io/sui/sui-eval/tests/fixtures/lang/
```

Record the source rev in this file when you do the import. The
hand-curated fixtures already present are a superset by design —
keep them even after vendoring.

## Hand-curated fixtures (2026-04-06)

The `.nix` files below were written by hand to cover semantics sui
must honor. They are not a substitute for the full CppNix corpus,
but they're enough to wire up the harness.

- `eval-okay-arith-precedence.nix` — operator precedence on ints
- `eval-okay-arith-mixed.nix` — int/float mixing rules
- `eval-okay-attrset-nested.nix` — dotted attr construction
- `eval-okay-attrset-inherit.nix` — inherit from a parent scope
- `eval-okay-attrset-rec.nix` — self-referential rec bindings
- `eval-okay-list-ops.nix` — builtins.length / head / tail / elemAt
- `eval-okay-list-concat.nix` — `++` chaining
- `eval-okay-let-chain.nix` — multi-binding let
- `eval-okay-let-shadow.nix` — name shadowing from outer to inner let
- `eval-okay-with-chain.nix` — nested `with` scopes
- `eval-okay-functions-currying.nix` — curried functions
- `eval-okay-functions-pattern.nix` — pattern-destructuring args
- `eval-okay-functions-defaults.nix` — default values in pattern args
- `eval-okay-if-else-chain.nix` — nested if/then/else
- `eval-okay-comparison-numeric.nix` — int comparison outcomes
- `eval-okay-comparison-string.nix` — lex string comparison
- `eval-okay-string-interp.nix` — multi-interp concatenation
- `eval-okay-builtin-map.nix` — builtins.map over [1 2 3]
- `eval-okay-builtin-filter.nix` — builtins.filter positive
- `eval-okay-builtin-foldl.nix` — foldl' sum
- `eval-okay-builtin-concatlists.nix` — flatten 2d list
- `eval-okay-builtin-attrnames.nix` — sorted attr name list
- `eval-okay-builtin-listtoattrs.nix` — list-to-attrs basic
- `eval-okay-type-of.nix` — typeOf of each primitive
- `eval-okay-json-roundtrip.nix` — fromJSON . toJSON identity

Adding a new case:

1. Write `eval-okay-<name>.nix` with the expression.
2. Run `nix-instantiate --eval --json --strict <path>` or mentally
   compute the expected JSON.
3. Put the exact JSON in `eval-okay-<name>.exp` — no trailing
   newline is required but is accepted.
4. Re-run `cargo test -p sui-eval --test lang_corpus`.
