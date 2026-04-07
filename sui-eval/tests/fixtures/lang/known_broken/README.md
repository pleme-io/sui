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

## Promotion back to the main corpus

When a bug is fixed, move the `.nix` + `.exp` files back out of
`known_broken/` into `../` (the parent `lang/` directory) — the
runner in `tests/lang_corpus.rs` will automatically pick them up
on the next test run.
