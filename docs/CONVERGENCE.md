# CONVERGENCE — the node is kept always rebuilt into place

> Grounded 2026-07-17. The **destination** (Operating Principle #0) stated
> first; then the phased path; then a tier-honest ledger that never rounds up.
> This is the runtime companion to [`LINK-GRAPH.md`](./LINK-GRAPH.md) (the
> ACTIVATE stage's link model) and [`SUI-SUPREMACY-ROADMAP.md`](./SUI-SUPREMACY-ROADMAP.md)
> (the eval→build→activate critical path this loop runs forever).

## 0. The destination

**A sui service on this node continuously reconciles the running system to the
toplevel its flake declares — deterministic, async, and streaming.** Where
`sui system rebuild switch --flake .#cid` is a *one-shot* `(evaluate → build →
activate)`, the reconcile loop is that step run **forever**: it watches the
flake source, re-evaluates via sui's own evaluator over the shared `/nix/store`,
and — whenever the declared toplevel drifts from what is activated — converges
once, streaming the atomic profile-generation swap into place. At the fixpoint
it holds and does nothing; on a source change it converges again. The node is
therefore *always* rebuilt into place, with no operator invocation.

This is **the Viggy Method (★★ CONTINUOUS CONVERGENCE — controllers, not
runbooks) applied to this node's own OS state.** The promessa is
`(defpromessa system-in-place)` — *"the node's active system generation equals
the toplevel its flake declares"* — and the controller proves, tick by tick and
attested, that it is holding it.

Three adjectives, made precise:

| Word | What it means here |
|---|---|
| **streaming** | An FSEvents watcher on the flake directory fires a reconcile the *instant* a `*.nix` / `*.lock` source file changes — plus a periodic interval tick that catches out-of-band drift. |
| **deterministic** | Same flake source → same evaluated toplevel → same content-addressed store path → same activation. The Diff compares store *basenames* (prefix-independent identity, `LINK-GRAPH.md` I9), so a redundant tick is a provable no-op — the loop holds at the fixpoint, converges exactly once per real change. |
| **async** | The whole loop is `tokio` — a `tokio::select!` over the FSEvents stream, the interval, and a shutdown signal; the eval/build/activate seam is `async`; a launchd/systemd unit runs it as a service. |

The **streamed link change** is real and atomic: each converge calls the shipped
`ProfileManager::set` (tmp-symlink + `rename(2)`, atomic within one filesystem —
`LINK-GRAPH.md` §2.3 / I5), advancing `/nix/var/nix/profiles/system →
system-N-link → <toplevel>`; the closure's own activate script then swaps
`/run/current-system` and the `/etc` farm. No reader ever observes a
half-switched system.

## 1. The seven-beat tick (the Viggy shape)

Each reconcile tick runs the Viggy seven-beat over a mockable Environment seam:

```text
Observe ─ desired = build_toplevel(flake); current = active generation ─▶ SystemObservation
Diff    ─ basename(desired) vs basename(current) ──────────────────────▶ drifted?
Classify─ SystemInPlace.evaluate(obs) ─────────────────────────────────▶ ReconcileVerdict
Decide  ─ Hold | Converge | Shadow | BuildOnly | Unobservable + a Dag ──▶ ReconcileDecision
Act     ─ drive the converge Dag ⇒ env.converge (atomic profile swap) ──▶ ActOutcome
Attest  ─ append the tick to a BLAKE3 OutcomeChain ─────────────────────▶ head id
Tick    ─ Requeue(interval) — a reconciler is never one-shot Done ──────▶ ReconcileOutcome
```

- **Held (in place):** `basename(desired) == basename(current)` → converge
  nothing. The steady state is quiet + cheap (eval is BLAKE3-cached).
- **Drifted + a mutating action (`Switch`/`Boot`/`Test`):** converge — activate
  the desired toplevel, stream the profile swap.
- **Drifted + `DryActivate` (the shadow floor):** build + diff the desired
  toplevel, activate *nothing* (breathe's shadow-first gate) — the safe default.
- **Unobservable** (the desired could not be built, or the profile could not be
  read): converge nothing, attest the honest `unobservable` verdict — a
  transient error can neither satisfy nor falsely fail the promessa, and **cid is
  never touched on a bad eval.**

## 2. Reuse, never re-roll (Operating Principle #1)

The loop owns **almost no new algebra** — it is a composition index over shipped
substrate:

| Layer | Reused primitive | Where |
|---|---|---|
| The seven-beat contract | `sui_supercacheci::controller::{Controller, ReconcileOutcome, ReconcileResult, ReconcileReport}` — verbatim | `sui-supercacheci` |
| Observe (build the desired toplevel) | `SystemOrchestrator::build_toplevel` (extracted from `rebuild_native`) | `sui-orchestrate/system.rs` |
| Act (activate — the atomic link swap) | `SystemOrchestrator::activate` + `ProfileManager::set` | `sui-orchestrate/system.rs` + `sui-store/profile.rs` |
| Observe (current generation) | `ProfileManager::list_generations` | `sui-store/profile.rs` |
| The converge STEP | `shigoto_dag::Dag` of a typed apply-generation Job | `shigoto-dag` |
| Config | `shikumi::TieredConfig` (default ← discovered ← override, hot-reload) | `shikumi` |
| Attestation | a BLAKE3 content-addressed `OutcomeChain` | this crate + prewarmer's twin |
| Streaming trigger | `notify` FSEvents on the flake dir | `notify` |

`SystemReconciler` is the **third** consumer of the fleet `Controller` contract
(after super-cache-ci itself and the dockerfile-prewarmer's layers-stay-warm
loop) — the three-site threshold that justifies its named destination:
extracting the trait to a shared leaf crate both `engenho` and `sui` implement.

The **two load-bearing extractions** in `system.rs` — `build_toplevel` (the
eval+build Observe primitive) and `activate` (the Act primitive) — are *not*
band-aids: they lift the reusable core out of `rebuild_native` so the one-shot
rebuild and the continuous loop share the exact same pipeline (solve once, in
one place). `rebuild_native` now composes them; its only behaviour change is a
malformed-flake edge (a non-derivation value) surfacing as `Err` instead of
`Ok(success:false)` — strictly more correct, and pinned by no test.

## 3. The Environment seam (the default delivery method)

Every side effect lives behind one injectable trait,
`reconcile::env::ReconcileEnvironment`:

```rust
async fn desired_toplevel(&self, config) -> Result<String, ReconcileError>;   // eval + build
async fn current_toplevel(&self) -> Result<Option<ActiveGeneration>, _>;      // active gen
async fn converge(&self, config, desired) -> Result<ConvergeReceipt, _>;      // activate (atomic swap)
```

The live impl (`LocalReconcileEnv`) delegates to the shipped orchestrator +
profile manager. Tests drive `MockReconcileEnv` — **no eval, no build, no
`/nix`** — so the whole seven-beat brain is proved mock-green (the TYPED-SPEC +
INTERPRETER Environment seam, applied to a controller instead of a spec
interpreter). This is why a build failure attests `unobservable` and a converge
failure attests `failed`, both without killing the loop and both under test.

## 4. The service (streaming, on this node)

`sui system converge [--flake .#cid] [--watch] [--interval-secs N] [--once]
[--action switch|dry-activate|boot|test]` runs the driver:

- `--once` — a single reconcile pass (the CI / manual form).
- `--watch` — the streaming daemon: `tokio::select!` over the FSEvents stream +
  the interval tick + `SIGINT`/`SIGTERM`, coalescing a burst of FSEvents into one
  tick, until a graceful shutdown.

On cid (darwin) it runs as a **launchd daemon** generated from Nix (the
darwin-module surface + a `KeepAlive` plist); on a NixOS node, a systemd unit.
The service scaffold ships in `contrib/`; wiring it into cid's private nix config
is the follow-up (a separate repo).

## 5. Tier-honest ledger (never round up)

| Row | Claim | Tier | Note |
|---|---|---|---|
| R1 | The seven-beat reconcile loop (promessa, drift math, Decide, shigoto converge Dag, BLAKE3 attestation) | **SHIPPED + tested** | mock-green over `MockReconcileEnv`; `cargo test -p sui-orchestrate` |
| R2 | `build_toplevel` / `activate` reusable seams | **SHIPPED** | extracted from `rebuild_native`, which now composes them |
| R3 | The streaming driver (FSEvents + interval + signal, coalescing) | **SHIPPED** | loop unit-tested over a hand-fed channel; the notify/signal edge is thin + integration-exercised by the CLI |
| R4 | `ReconcileConfig` as a `shikumi::TieredConfig` | **SHIPPED (schema)** | `bare()` = safe shadow floor, `prescribed_default()` = live in-place. File-discovery wiring in the CLI is a follow-up (`pending-shikumi`); the CLI builds the config from flags today. |
| R5 | A **byte-identical cid** convergence | **GATED on M2.6** | rides the *exact same* module-system-fixpoint gate as `rebuild_native` (`SUI-SUPREMACY-ROADMAP.md`). The loop is real + tested; the byte-identical *result* is blocked upstream — never conflated. |
| R6 | Attestation identity | **hash chain, unsigned** | BLAKE3 content-addressed + tamper-evident; the Ed25519 signing rung is the named destination (`signature: None` slot present). |
| R7 | Native activation link-graph (own the `/etc` farm + `/run/current-system` swap) | **DELEGATED** | today the closure's own `/activate` script does it (correct-by-delegation for the cid proof); `LINK-GRAPH.md` L4 makes it sui-native — the day the converge Dag grows from 1 node to the ordered `/etc`→launchd→current-system sub-steps. |
| R8 | The launchd/systemd service wired onto cid | **FOLLOW-UP** | the binary + service scaffold ship here; enabling it in cid's private nix config is a separate-repo change. |

**The one sentence that must not be rounded up:** the reconcile *loop* is real,
tested, and shipped; a *byte-identical* continuous cid convergence is gated on
M2.6 exactly as the one-shot rebuild is — the loop inherits that gate, it does
not remove it.

## 6. Composition with the doctrines

- **★★ CONTINUOUS CONVERGENCE / PROVABLE OUTCOMES (the Viggy Method)** — this is
  the local-node realization: a `(defpromessa)` reconciled by a Controller with a
  continuously-renewed (BLAKE3) attestation chain.
- **★★ Shigoto** — the converge STEP is a typed `Dag`, ready for L4's ordered
  activation sub-steps.
- **★★ TYPED EMISSION** — every log/note is a `write!` inside a `Display`
  (`TickNote`), never `format!()` of a report string.
- **★★ UNREPRESENTABILITY** — the fail-closed root gate (`activate_system`) means
  an unprivileged reconcile tick has *no code path* that mutates the live system;
  a shadow (`DryActivate`) tick makes no converge call at all.
- **★ Stand on real ground** — each converge is a byte proven against the store +
  an atomic swap + an attested link; the loop stands on the ledge it just laid.
