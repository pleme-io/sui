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
JSON-eval — 112 candidates. **sui passes 72** (now active in `../`); the **65**
gaps parked here are the language frontier — expand the active corpus by closing
them. This turned the language test surface from 25 curated fixtures into a
measured 72/(72+65) coverage number against Nix's own tests.

**The one HANG (a real bug, not just a value divergence): `eval-okay-deepseq`.**
sui's library eval path (`sui_eval::eval` + `to_json`) effectively hangs (>60s) on
it while the `sui` binary's `eval --json` path returns fast — a genuine divergence
between the two eval entries around `builtins.deepseq` / the deep-force in
`to_json`. Parked here so it can't wedge the corpus gate; fixing it is a tracked
task (the two paths must converge).

## Promotion back to the main corpus

When a bug is fixed, move the `.nix` + `.exp` files back out of
`known_broken/` into `../` (the parent `lang/` directory) — the
runner in `tests/lang_corpus.rs` will automatically pick them up
on the next test run.
