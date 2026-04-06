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

## `eval-okay-attrset-nested.nix` — sui drops earlier dotted bindings

Expression:
```nix
{ a.b.c = 1; a.b.d = 2; a.e = 3; }
```
Expected (`nix-instantiate`): `{"a":{"b":{"c":1,"d":2},"e":3}}`
Actual (sui):                  `{"a":{"e":3}}`

Root cause hypothesis: sui's attrset construction replaces `a` with
its latest dotted assignment instead of merging them. This is a
hot-path semantics bug — every nixpkgs module uses this pattern.

## `eval-okay-with-chain.nix` — `with` scope shadowing is inverted

Expression:
```nix
with { x = 1; }; with { x = 2; }; x
```
Expected (`nix-instantiate`): `2`
Actual (sui):                  `1`

Root cause hypothesis: sui's `with` pushes the new scope *below* the
existing `with` scopes (wrong) instead of *above* them, so `x` still
binds to the outermost `with`. The correct semantics is that an
inner `with` shadows an outer one for the duration of its body.

## Promotion back to the main corpus

When either bug is fixed, move the two files back out of
`known_broken/` into `../` (the parent `lang/` directory) — the
runner in `tests/lang_corpus.rs` will automatically pick them up
on the next test run.
