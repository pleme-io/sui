# `sui flip-probe` — the receipt

`sui flip-probe <flake>#<attr>` evaluates `<attr>.drvPath` with sui and with
CppNix, each in its own child process, and writes one JSON receipt. When the two
drvPaths differ it walks both `.drv` graphs to the first drv that differs while
all its same-name inputs agree, and records which fields differ there.

A gate that decides whether sui may drive a machine's rebuild reads this
receipt, so its shape is a contract. This page documents it; the test
`flip_probe::tests::receipt_schema_is_stable` fails if a field is added, removed
or renamed without `schema` moving to a new version.

## Usage

```
sui flip-probe <flake>#<attr> [--nix <path>] [--receipt <file>] [--max-nodes <n>]
```

| flag | meaning |
|---|---|
| `--nix` | the CppNix `nix` binary; default: the installed one, found by absolute path (`/nix/var/nix/profiles/default/bin`, `/run/current-system/sw/bin`, then `PATH`; `SUI_CPPNIX_BIN_DIR` overrides) |
| `--receipt` | write the receipt to a file; default stdout |
| `--max-nodes` | drv-pair budget for the graph walk (default 200000); a walk that hits it sets `divergence.truncated` |

Exit status is 0 only for `verdict = "match"`. The receipt is written either way.

Commands run, in order:

1. `sui eval --no-eval-cache --raw <flake>#<attr>.drvPath` (the tree-walker; the
   eval cache is bypassed so every `.drv` is instantiated and walkable)
2. `nix --version`, then
   `nix eval --extra-experimental-features 'nix-command flakes' --raw <flake>#<attr>.drvPath`
3. `nix flake metadata --json …` for the flake's identity

Wall time is measured around each child; peak RSS comes from `wait4`'s
`rusage` for that child alone.

## Schema `sui.flip-probe/v1`

All keys are camelCase. `null` means "not known", never "zero".

| key | type | meaning |
|---|---|---|
| `schema` | string | `"sui.flip-probe/v1"` |
| `installable` | string | as given |
| `flakeRef` | string | the part before `#` |
| `attrPath` | string | the part after `#`, without `.drvPath` |
| `flake.rev` | string \| null | locked git revision (null for dirty or non-git sources) |
| `flake.dirtyRev` | string \| null | the revision a dirty tree was taken from |
| `flake.narHash` | string \| null | locked NAR hash, when CppNix reports one |
| `flake.sourcePath` | string \| null | the store path CppNix copied the source to (content-addressed) |
| `host.platform` | string | `<arch>-<os>` of this sui build, e.g. `aarch64-macos` |
| `startedAtUnix` | integer | Unix seconds at start |
| `sui`, `cppnix` | EngineRun | one per engine |
| `verdict` | string | `match` \| `diverge` \| `suiFailed` \| `cppnixFailed` \| `bothFailed` |
| `divergence` | Divergence \| null | present exactly when `verdict` is `diverge` |

**EngineRun**

| key | type | meaning |
|---|---|---|
| `engine` | string | `sui` or `cppnix` |
| `version` | string | `sui <version>`, or `nix --version` output |
| `argv` | string[] | the exact command line |
| `drvPath` | string \| null | the printed drvPath; null unless the run exited 0 and printed a `/…​.drv` path |
| `exitCode` | integer \| null | null when killed by a signal |
| `signal` | integer \| null | terminating signal |
| `wallSeconds` | number | wall-clock seconds |
| `maxRssBytes` | integer | peak resident set size, bytes on every OS |
| `stderrTail` | string | last 8 KiB of stderr when the run failed; empty otherwise |

**Divergence**

| key | type | meaning |
|---|---|---|
| `first` | DivergentDrv \| null | the shallowest frontier drv (breadth-first from the top); null when the walk reached none, see `missing` and `unreadable` |
| `frontierCount` | integer | frontier drvs found in total |
| `frontier` | DrvPair[] | up to 50 frontier pairs after `first` |
| `missing` | {parent, name, countSui, countCppnix}[] | input names present a different number of times on the two sides |
| `unreadable` | {path, side}[] | drvs the walk could not read (`side` is `sui` or `cppnix`) |
| `visited` | integer | drv pairs visited |
| `truncated` | boolean | the walk stopped at `--max-nodes`; the frontier is partial |

**DivergentDrv**: `name` (drv name without hash), `trail` (names from the top
drv down to this one), `suiDrv`, `cppnixDrv`, `fields` (FieldDiff[]).

**DrvPair**: `name`, `suiDrv`, `cppnixDrv`.

**FieldDiff** compares parsed ATerm fields, never renderings:

| key | type | meaning |
|---|---|---|
| `field` | string | `outputs.<o>.path` \| `outputs.<o>.hashAlgo` \| `outputs.<o>.hash` \| `outputs.<o>` (present on one side) \| `inputDrvs` \| `inputSrcs` \| `system` \| `builder` \| `args` (lengths differ) \| `args[<i>]` \| `env.<name>` |
| `sui` | string \| null | sui's value; null when absent on sui's side |
| `cppnix` | string \| null | CppNix's value; null when absent on CppNix's side |
| `hashCascadeOnly` | boolean | the values are equal once every store hash is replaced by a placeholder: the difference was inherited, not made here |

Fields are listed in the order outputs, inputDrvs, inputSrcs, system, builder,
args, env. `inputDrvs` renders as space-separated `<path>!<out,…>`;
`inputSrcs` as space-separated paths.

## Reading a receipt

A gate that requires sui's drvPath to equal CppNix's reads `verdict` and
`sui.drvPath`; the other fields are the evidence for a human when it fails.
`sui::flip_probe::read_receipt` parses a receipt and refuses any other schema
version; unknown keys are rejected.
