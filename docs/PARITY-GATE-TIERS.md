# Parity gate: tier-honest status and the oracle campaign

Relocated from CLAUDE.md (North star section). The rules stay in CLAUDE.md.

**Tier-honest on the gate itself (2026-07-18 — never round up the enforcer).**
The corpus is *split* by where it actually proves anything, and the doc must say
so. The ~40 pure-builtins rows (hash / derivation / context / attrset-merge
algebra) are environment-independent — genuine CI theorems, reproducible on any
runner. The ~35 ecosystem rows (`hello`/`stdenv`/`neovim`/… + the darwin
`currentSystem` seeds) import **impure `<nixpkgs>`** — an *unpinned,
machine-dependent* oracle — so on CI (which resolves a different nixpkgs rev than
the one those rows were byte-closed against) sui **errors** on them and they count
as regressions. Net: `sui parity` was **green locally (against the operator's
`<nixpkgs>`) but inert on CI — 0 successes in the last 40 runs — and releases
cut through the red.**

> **★ CORRECTED 2026-08-09 — the paragraph above is STALE and destination (3)
> ALREADY SHIPPED. Re-measured, do not plan against the old text.** The corpus
> is **77/77 sealed, 0 regressions** (`sui parity`, identical with and without
> `SUI_PARITY_PUREONLY`), and `.github/workflows/parity.yml` already
> implements the per-row-skip design described below as pending:
> `SUI_PARITY_PUREONLY=1` reclassifies rows that *cannot evaluate* as Skipped,
> while a wrong byte (`Diverge`) still fails. So the gate BLOCKS on the
> environment-independent rows rather than being inert. Destinations (1)
> flake-locked oracle and (2) per-row `EnvCapability` remain open.
>
> **And the more important correction: a sealed corpus is not evidence the
> evaluator is correct.** The same day this was re-measured, evaluating the
> operator's REAL fleet flake found two silent divergences the 77 rows pass
> straight over — one of which renamed every NixOS system. The corpus is
> shaped so it cannot see the failure classes the flip depends on. Read
> [`docs/SILENT-DIVERGENCE-HUNT.md`](docs/SILENT-DIVERGENCE-HUNT.md) before
> planning any flip work; it carries the instrument, not just the status. A gate cannot be a "theorem re-proven on every change" while
its oracle floats: **★★ DETERMINISTIC INSTANTIATION is violated at the gate's own
ground truth,** and a chronically-red gate is *worse than none* because it blinds
the ~40 rows that would otherwise catch a real regression on every push. The
destination (one campaign, shared with the darwin/eval-memory work and
super-cache-ci's hermetic oracle): (1) the corpus imports a **flake-locked**
nixpkgs, not `<nixpkgs>`, so every machine evals the same bytes and red means a
*real* sui divergence; (2) each row carries a typed `EnvCapability` (arch + memory)
so it runs only where it can and **skips honestly** elsewhere (a linux runner skips
the darwin rows; a small runner skips the OOM-heavy rows) instead of
`sui-err`-as-regression; (3) CI then proves the reproducible subset as a
**blocking** gate and a pinned-oracle big-mem job proves the full corpus. The law
this taught, fleet-wide: *a seal is only real if its oracle is reproducible where
the seal is enforced.* Tracked as the parity-gate task.
