# Known-broken language fixtures

Each `.nix` file here is a hand-written case that sui evaluates
incorrectly compared to real Nix (`nix-instantiate --eval --json
--strict`). They are held out of the main `lang_corpus` harness so
the rest of the suite stays green, but they are not deleted — the
point is to have a failing test ready to flip back to green the
moment the underlying sui bug is fixed.

To reproduce on the current sui:

```bash
nix-instantiate --eval --json --strict \
  sui-eval/tests/fixtures/lang/known_broken/eval-okay-<name>.nix
# then:
cargo run -p sui eval -- -E "$(cat sui-eval/tests/fixtures/lang/known_broken/eval-okay-<name>.nix)"
```

## Resolved

### `eval-okay-attrset-nested.nix` (FIXED)

Promoted back to `../` (the main `lang/` corpus). The bug was that
sui's attrset construction replaced `a` with its latest dotted
assignment instead of merging. Fixed by `merge_nested_insert` in
`eval_attrset`.

### `eval-okay-with-chain.nix` (FIXED)

Promoted back to `../` (the main `lang/` corpus). The bug was that
sui's `with` pushed new scopes below existing ones. Fixed by
correct scope stacking in the `With` handler.

## 2026-07-22 — vendored CppNix corpus gaps (STRATOSPHERE M3)

Beyond the hand-written cases above, this dir now also holds `eval-okay-*` pairs
vendored from CppNix's own functional corpus (`nix/tests/functional/lang/`,
NixOS/nix default branch, sparse-checkout) that the **local `nix-instantiate`
oracle (Nix 2.34.7) JSON-evaluates but sui does not yet match**. The `.exp` were
regenerated from the *local* oracle (`--eval --json --strict`) so they are exact
for this machine, not the upstream `.exp` (which can drift).

Of 144 upstream eval-okay tests: 13 need `.flags` args, 19 the local oracle can't
JSON-eval — 112 candidates. **sui passes 73** (now active in `../`); the **64**
gaps parked here are the language frontier — expand the active corpus by closing
them. This turned the language test surface from 25 curated fixtures into a
measured 73/(73+64) coverage number against Nix's own tests.

### `eval-okay-deepseq` — FIXED + graduated (2026-07-22)

The one HANG surfaced by the M3 expansion. sui's library deep-force
(`builtins/control.rs::deep_force`) recursed into a self-referential attrset
(`let as = { x = 123; y = as; }; in as`) forever — `stacker::maybe_grow` turned
the infinite recursion into an unbounded stack-grow HANG (>60s), not a prompt
overflow. Fixed by adding an `Rc`-identity seen-set (`deep_force_seen`), mirroring
cppnix's `forceValueDeep` `std::set<const Value*> seen`; the cyclic value is now
finite and deepSeq returns 456 as nix does. Graduated back to `../`; locked by
`builtins_deep_seq_cyclic_{attrset,list}_terminates` +
`builtins_deep_seq_still_forces_a_nested_throw`.

## Promotion back to the main corpus

When a bug is fixed, move the `.nix` + `.exp` files back out of
`known_broken/` into `../` (the parent `lang/` directory) — the
runner in `tests/lang_corpus.rs` will automatically pick them up
on the next test run.
