# sui as the fleet's builder platform — naturalize(the nix builder)

**Status: DESIGN + a partly-shipped floor. Read the ledger before citing any
row of this document as a capability.**

## The destination, unhedged

Every build in the fleet is executed by sui: a typed `Builder` that **cannot be
constructed without a deadline**, dispatching to local sandboxes and remote
machines through one typed `BuildSite` vocabulary, where a dead builder is a
typed error in milliseconds and a stalled build is a typed error at its bound —
never a wait. `nix build`'s scheduler becomes an implementation sui speaks to,
not the thing the fleet depends on.

## Why this came up (the receipt, cid 2026-08-06)

`nix build .#darwinConfigurations.cid…toplevel` sat **27 minutes at 0.0% CPU**.
No builders, no rustc, no build directories, no sockets. It had dispatched an
`x86_64-linux` derivation to `linux-builder`, whose LaunchDaemon plist is on
disk but was never loaded by launchd, so ssh answers `Permission denied`. nix
did not fail, and did not fail over to the healthy `rio` in the same
`buildMachines` list. It blocked.

The root cause is not the dead builder — builders die. It is that **nix's build
scheduler has no typed notion of a bound**: `max-silent-time` and `timeout`
both default to `0`, and in nix `0` means *no limit*. The unbounded build is
the default posture, and it is *representable* at every layer.

## Recon: what sui already has (find-don't-build)

Measured by reading source on 2026-08-06, not inferred from names.

| Capability | Where | Reality |
|---|---|---|
| Local sandboxed build | `sui-build/src/local_builder.rs`, `sandbox.rs` (Darwin + Linux sandboxes) | **SHIPPED** |
| Closure realization, substitute-first | `sui-build/src/closure.rs`; `Substitutor` pulls cached paths so the builder only builds the remainder | **SHIPPED** |
| Reference scanning | `sui-build/src/reference_scan.rs` | **SHIPPED** |
| **Bounded realize — local path** | `sui-orchestrate/src/system.rs:947`, `tokio::time::timeout(bound, build)`; the error text literally says *"local build stalled … NOT a hang"* | **SHIPPED** |
| **Bounded realize — daemon path** | `sui_store::realize_via_daemon_bounded(&store, drv, out, bound)` | **SHIPPED** |
| Content-addressed store + cache | `sui-store`, `sui-cache` (now with a typed `NarCodec`) | **SHIPPED** |
| Build-state machine | `BuildState` + `transition()` in `sui-build/src/traits.rs` | **SHIPPED** |
| **Remote build** | `BuildError::NotImplemented("remote build")` | **ABSENT — a named gap** |
| Builder selection / failover across N machines | — | **ABSENT** |
| `SandboxError::Timeout(u64)` | `sandbox.rs:170` | **DECLARED, NEVER CONSTRUCTED** outside one test at `:1337`. Do not cite it as enforcement. |

**So sui already solves, at the orchestrate layer, the exact class that hung
cid** — and solves it better than nix does, because the bound is present on
*both* realize paths and the failure carries a message that tells the operator
what it means. That is a real, shipped advantage, and it is worth stating
plainly.

**And sui cannot be the primary builder platform today**, because the one
capability cid needed — dispatching an `x86_64-linux` derivation to another
machine — is `NotImplemented`. Those two sentences are both true. Rounding the
first into "sui replaces the nix builder" is exactly the round-up this
repository's parity discipline exists to prevent.

## The load-bearing design decision

The bound is currently a **call-site discipline**, not a type. `Builder::build`
is:

```rust
pub trait Builder: Send + Sync {
    async fn build(&self, drv: &Derivation) -> Result<BuildResult, BuildError>;
    async fn output_exists(&self, path: &StorePath) -> Result<bool, BuildError>;
}
```

Nothing in that signature mentions time. Both current call sites happen to wrap
it, which is *correct today and unenforced tomorrow* — the third call site is
one refactor away from being unbounded, and it will look exactly as reasonable
as the first two. This is the same shape as nix's `0`: the unbounded build is
representable, so eventually it gets written.

**The destination is to move the bound into the trait**, so a `Builder` that
can hang has no constructor:

```rust
pub trait Builder: Send + Sync {
    /// Every build carries its deadline. There is no unbounded overload —
    /// that is the point, not an inconvenience.
    async fn build(&self, drv: &Derivation, budget: BuildBudget)
        -> Result<BuildResult, BuildError>;
}
```

with `BuildBudget` a `Refined`-style newtype whose constructor refuses zero, so
"no limit" has no spelling. That upgrades the tier from *only-mitigated (C4 —
holds only while every call site remembers)* to *parse-time-rejected*.

## Phased path down from the destination

- **M0 — the bound becomes a type.** `BuildBudget` newtype + `Builder::build`
  takes it; both existing call sites pass what they already compute. No
  behavior change, no new capability; the win is that unbounded stops being
  writable. One red run: a `BuildBudget::new(0)` must not compile-or-construct.
- **M1 — `BuildSite` vocabulary.** A typed enum over `Local(Sandbox)` and
  `Remote(SshHost)`, with **liveness probed before dispatch** and a typed
  `SiteUnreachable` error. This is the row that would have turned cid's 27
  minutes into a sub-second failure.
- **M2 — remote build.** Fill `NotImplemented("remote build")` behind the M1
  vocabulary: copy closure, execute, copy back, all budget-bounded.
- **M3 — failover.** Given N sites for one system, an unreachable site is
  skipped rather than blocked on. cid declares two `x86_64-linux` builders and
  nix picked the dead one; the correct behavior is to try the live one.
- **M4 — primary.** Only once M1–M3 hold as a green differential against
  `nix build` on a real closure may this document drop the word "design".

## The interim, already landed

`nix` repo `modules/pleme/shared/build-liveness.nix` bounds
`max-silent-time`/`timeout` fleet-wide, so **today's** nix-driven builds fail
instead of hanging. That is a bound *around* nix, not a fix *of* nix's model —
an interim on the shortest path to M1, named as such rather than mistaken for
the destination.

<!-- tier-ledger -->

| nix-builder capability | sui realization | tier |
|---|---|---|
| build with no time limit (`timeout = 0`) | `BuildBudget` newtype refusing zero — "no limit" has no spelling (M0, DESIGN) | parse-time-rejected |
| unbounded local realize | SHIPPED-composition: `tokio::time::timeout(bound, build)` on both realize paths, typed `RebuildFailed` naming the bound | only-mitigated (C4 — the bound is a call-site wrap, not in the `Builder` signature; a new call site can omit it) |
| dispatch to a dead remote builder and block | NET-NEW `BuildSite` with pre-dispatch liveness + typed `SiteUnreachable` (M1, DESIGN) | only-mitigated (C2 — reachability is an external-world fact; a site can die between probe and dispatch, so the budget stays the backstop) |
| remote build execution | `BuildError::NotImplemented("remote build")` (M2, ABSENT) | only-mitigated (C6 — not built; the honest floor is that nix does this and sui does not) |
| pick a live builder among N | NET-NEW failover over `BuildSite` (M3, DESIGN) | only-mitigated (C2 — same external-world ceiling) |
| sandbox execution timeout | `SandboxError::Timeout(u64)` — **declared, never constructed** outside a test | only-mitigated (C6 — a variant with no producer is not enforcement) |
| content-addressed store + substitute-first | SHIPPED: `sui-store` + `Substitutor` | parse-time-rejected |

**Retire nothing on the strength of this document.** Every `only-mitigated` row
above names its ceiling, and three of them are `C6` — meaning not built. sui is
a *resident* of the builder problem, not yet its citizen: rebuilt-not-vendored,
tier-labelled, and honestly short of parity.
