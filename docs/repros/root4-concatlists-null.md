# M2.6 ROOT #4 — `concatLists: expected list, got null` (field-lazy fixpoint)

## The failing case (the ONLY known reproducer — requires the full base-module set)

```sh
sui eval --no-vm --impure --raw '
  let n = builtins.getFlake "github:NixOS/nixpkgs/b77b3de";
  in (n.lib.nixosSystem { system = "x86_64-linux"; modules = []; }).config.system.name'
# nix → "nixos"
# sui → Eval(TypeError("expected list, got null
#          while calling the 'concatLists' builtin (lib/modules.nix)
#          while calling the 'seq' builtin (lib/modules.nix)"))
```

`seq` = `checked = seq checkUnmatched` (lib/modules.nix:337). `checkUnmatched`
forces `merged.unmatchedDefns` (line 900 `concatLists (mapAttrsToList … unmatchedDefnsByName)`),
whose deep `mergeModules'` recursion forces `pushDownProperties`/`atDepth`
(the `doRename`/`setAttrByPath` carrier), which forces a `mkIf`'s content that
reads `config.services.<x>` mid-fixpoint → the softening returns `null` → a
list-typed position gets `null` → `concatLists` throws.

## The confirmed mechanism (byte-verified with a hard-soften discriminator)

`SUI_M26_HARDSOFTEN=services` changes the outcome → a `config.services.borgbackup`
select-miss softening (`missing-key=services` in borgbackup.nix, i.e.
`config.services.borgbackup` where `config` is the empty-Promise partial) is on
the failing path. borgbackup's config is:

```nix
config = lib.mkIf (with config.services.borgbackup; jobs != {} || repos != {}) (
  with config.services.borgbackup;
  { users = lib.mkMerge (lib.mapAttrsToList mkUsersConfig repos); … });
```

`pushDownProperties` (lib/modules.nix:1282) forces this `mkIf`'s `content`
UNCONDITIONALLY (to enumerate its keys for the `byName` collection — cppnix does
the same). Forcing the content forces `with config.services.borgbackup`.

- **cppnix:** `config` is a lazy attrset whose `services` key resolves
  independently to the option default `{}` → `repos = {}` → `mkMerge []` → `[]`.
- **sui:** `config` is one monolithic `Promise` thunk; the re-entrant
  `config.services.borgbackup` read during `config`'s OWN force yields the empty
  partial → the `in_promise_eval` select-miss softening returns `null` →
  `repos = null` → `mapAttrsToList mkUsersConfig null` → `null` → `mkMerge null`
  → `concatLists null`.

This is NOT a dynamic-attrpath-key over-force (ROOT #1/#2/#3, all sealed). It is
the **field-independence gap** the historical doc names: the softening returns
`null` where cppnix resolves the field to its real (empty-list/empty-attrs)
default. The `doRename`/`atDepth` dynamic-head-key (`{ ${elemAt attrPath n} = … }`)
is only the CARRIER on the force chain — its key force is correct (cppnix forces a
dynamic head key too); the divergence is the softened `config.<x>` read.

## Why no minimal pure-lib repro exists

The empty-`Promise` partial only arises from the FULL base-module `_module.args`
fixpoint bootstrap re-entrance (bisection data points I/J: subsets give bounded
errors; only the full 1982-module list produces the empty partial). Every
smaller `lib.evalModules` construction resolves `config.<x>` correctly because
sui's fixpoint reaches WHNF before the re-entrant demand. Cases proven to NOT
diverge (return the same value as nix), isolating the boundary:

1. `mkRenamedOptionModule [old opt] [new opt]` + a use of `old.opt` → both `"fromold"`.
2. `doRename` into an `attrsOf submodule` `to` path + select an unrelated option → both `PROBE`.
3. `_module.args.derived = mkDefault (if config.enable then …)` fixpoint + `mkIf`-gated unmatched def → both agree.
4. `mkIf (config.svc != {}) (with config.svc; { … })` with `svc` `attrsOf str`/`attrsOf submodule` default `{}` → both `PROBE`.

## Diagnostics landed (byte-neutral, env-gated, zero hot-path cost)

- `SUI_M26_CLTRACE=1` — reports each non-list `concatLists` element's type + file stack.
- `SUI_M26_CLDUMP=1` (with `SUI_TRACE_EVAL=ring`) — ring-buffer tail at the null.
- `SUI_M26_HARDSOFTEN=<substr>` — throws a distinctive error at the select-miss
  softening whose demanded attrpath contains `<substr>` (the discriminator that
  pinned `services.borgbackup` on the failing path).
- `SUI_M26_MAXFRAMES=<n>` — configurable force-stack depth in error traces.

## Fix status

ROOT #4 is the deep field-lazy fixpoint change (`ThunkRepr::Promise` must resolve
the demanded field against the real merge lattice, not an empty sentinel). There
is NO safe narrow over-force to kill here (verified: `pushDownProperties` forcing
`mkIf` content and `atDepth` forcing its dynamic head key are both correct — cppnix
does the same). Not closed in this session; no parity-regressing hack shipped
(`sui parity` held at 35 match, `hello` drvPath byte-identical throughout).
