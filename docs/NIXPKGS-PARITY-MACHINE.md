# The nixpkgs-parity machine

**sui continuously, automatically, and publicly proves it tracks-and-builds
nixpkgs — byte-identically — and self-fixes when it doesn't.**

This is a *machine*, not a coverage number. The operator's law for it:

> Build and improve the machine; let coverage grow legitimately; never inflate
> "how far." When the machine finds a divergence, we **fix the machine** (the
> eval/build core) at its load-bearing cause — like the neovim/python27
> `callPackage` root (a stale with-scope-cache partial, closed 2026-07-15) — not
> paper over it. Whether the basket is 65 rows or 65,000 is an *output* we earn,
> one proven byte-verified row at a time.

## The loop (one sentence)

`tend` sees a new nixpkgs → bumps sui's tracked pin → the `pleme-io/actions`
parity pipeline runs **eval-parity + build-parity** vs the real `nix` → **tameshi**
attests the result → a public badge/receipt publishes → any divergence is a red
gate that **auto-files the diverging package as a tracked root** (the
neovim→closed workflow, now continuous and public).

## Two parity tiers (tier-honest — never conflate them)

| Tier | Claim | Mechanism | State |
|---|---|---|---|
| **EVAL-parity** | sui *evaluates* nixpkgs identically | `sui parity` — sui's computed `drvPath`/hash/NAR byte-diffed against real `nix`, per expression | **shipping** — 65 rows, `parity.yml` gate on every push; `nixpkgs-parity-track.yml` runs it daily vs latest nixpkgs |
| **BUILD-parity** | sui *builds* nixpkgs identically | realize a derivation with sui and byte-compare the built output (NAR) to nix's | **the genuinely-new harness (P3)** — a sampled, growing basket; starts at the stdenv bootstrap + a few leaves |

"sui builds **all** of nixpkgs" is **aspirational** and is never claimed until the
build-parity basket actually covers it. The honest, shippable claim is: *"sui
builds a continuously-growing, publicly-attested basket of nixpkgs, byte-identical,
auto-refreshed against the latest nixpkgs, and every gap is a tracked red gate."*

## Composition (mostly wiring shipping substrate)

| Piece | Source | State |
|---|---|---|
| `sui parity` eval corpus + `parity.yml` gate | sui | **shipping** (65 rows) |
| `nixpkgs-parity-track.yml` — daily eval-parity vs latest nixpkgs | sui | **shipping (P0, this commit)** |
| `tend` daemon + version-tracking + flake-update job graph | `pleme-io/tend` | **shipping** — extend by one `nixpkgs-track` job kind (P2) |
| `pleme-io/actions` reusable-workflow pattern + AUTO-RELEASE | `pleme-io/actions` + `substrate` | **shipping** — add `sui-eval-parity` / `sui-build-parity` actions (P1) |
| tameshi BLAKE3 + signed attestation | `pleme-io/tameshi` | **shipping** — wire to the parity run (P4) |
| **build-parity harness** (realize + NAR-compare) | sui (new) | **design (P3)** — the real new build |

## Phases

- **P0 (shipped):** `nixpkgs-parity-track.yml` — scheduled eval-parity vs latest
  `nixpkgs-unstable`, in sui's own CI. The machine's beating heart runs publicly
  today.
- **P1:** extract the parity run into `pleme-io/actions/sui-eval-parity` (a
  tlisp-backed reusable action) + a `substrate` reusable workflow; sui consumes it
  via a 3-line shim (the AUTO-RELEASE consumer pattern). *Merges it with
  pleme-io/actions development.*
- **P2:** `tend.nixpkgs-track` job kind — tend polls the nixpkgs channel ref (it
  already polls tags for akeyless-matrix) and dispatches the pipeline on a new
  commit, so tracking is daemon-driven, not only cron. *Bends tend to do it.*
- **P3:** the **build-parity basket** — a `sui build-parity` subcommand that
  realizes a growing set of derivations and byte-compares the NAR to nix's,
  starting at the stdenv bootstrap. Grows one byte-verified row at a time.

  **P3 findings (2026-07-15 — tier-honest split of "sui builds nixpkgs"):**
  - **Derivation-hashing / instantiation parity: PROVEN + continuously gated.**
    sui computes the `drvPath` AND the `outPath` byte-identical to nix — e.g.
    `sui build .#seed` computed `outPath =
    ddj3291990gsayjpv49c0qxc6n6k9ms5-bpf-seed`, identical to `nix build .#seed`.
    Since the output path is input-addressed from the whole `.drv` graph, this
    proves **sui produces the same build graph as nix** — the fundamental half.
    This is exactly what the 65-row `sui parity` corpus gates on every commit.
  - **Realization engine: works.** `sui-build` (`LocalBuilder` + `DarwinSandbox`
    + `build_closure`) realizes a derivation to its output; a `.drv` that is
    present in the store builds to the expected path.
  - **End-to-end realization: BLOCKED on daemon-mediated store writes (the P3
    build).** `write_derivation_to_store` (derivation.rs:126) writes the `.drv`
    via a direct `std::fs::write` to `/nix/store/…drv`; on a multi-user nix store
    `/nix/store` is `dr-xr-xr-x root` (read-only to the user), so the write fails
    to a fallback path and `BuildClosure::compute` can't read the `.drv` at its
    real store path. sui must write to the store **via the nix daemon protocol**
    (it has `sui-store`/`sui-daemon`, but the build path uses direct `fs::write`).
    This is the named, load-bearing P3 brick — realizing + NAR-comparing
    end-to-end needs daemon-mediated (or single-user/root) store writes.
  - Landed en route: a real `sui build` CLI fix — the flake-build path forces the
    `drvPath` thunk before extracting it (it silently printed the derivation
    instead of building, because `drvPath` is lazy).
- **P4:** tameshi-attested public receipt per nixpkgs commit + a README badge:
  `sui ⟷ nixpkgs@<sha>: N eval ✓ · M build ✓ · 0 diverge`.

## The self-fix contract (why this is a machine, not a dashboard)

A red run is not a failure to hide — it is the machine doing its job. The response
is fixed:

1. The diverging expression/package is **auto-filed as a tracked root** (a
   `KnownDiverge` corpus row / a `pending-parity:` note), exactly like neovim was.
2. It is **root-caused in the eval/build core** and fixed at the load-bearing
   cause — never a per-package workaround (the neovim fix was a general
   with-scope-cache correctness fix, which is why it can't recur).
3. The corpus **grows by one honest row**; the fix is sealed so a regression is a
   future red gate.

That loop — *find → root-cause → fix the machine → seal → grow* — is the whole
machine. Everything else (scheduling, attestation, badges) just makes it
continuous and public.

## Standing rule

Every change here advances a phase or leaves a typed `pending-parity-machine:
<phase>` note. Coverage claims are tier-labeled (eval vs build; basket size);
"all of nixpkgs" is never rounded up. The machine's value is the *accumulating
real ground* — each proven row is byte-verified against nix, committed, and
continuous.
